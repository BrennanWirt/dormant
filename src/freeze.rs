//! `sleep_mode = "freeze"`: find the backend's Java process, check it is safe to pause, and
//! SIGSTOP / SIGCONT it. Uses /proc and `sh`'s `kill` builtin only (no libc, no unsafe).
//!
//! A SIGSTOPped server keeps its RAM and its listening socket (the kernel still accepts TCP
//! connections), so dormant must not ping it while frozen; it answers from the cache instead.

use std::fs;
use std::time::Duration;

use tokio::process::Command;
use tokio::time::sleep;

use crate::Config;

pub struct Frozen {
    pub pid: u32,
    /// Arguments for `kill`: a negative number is a whole process group.
    pub targets: Vec<i64>,
}

/// (state, ppid, pgid) from the text of /proc/<pid>/stat. The command name may contain spaces and
/// parentheses, so fields are split after the last ')'.
pub fn parse_stat(s: &str) -> Option<(char, u32, u32)> {
    let mut f = s[s.rfind(')')? + 1..].split_whitespace();
    let state = f.next()?.chars().next()?;
    let ppid = f.next()?.parse().ok()?;
    let pgid = f.next()?.parse().ok()?;
    Some((state, ppid, pgid))
}

pub fn proc_stat(pid: u32) -> Option<(char, u32, u32)> {
    parse_stat(&fs::read_to_string(format!("/proc/{pid}/stat")).ok()?)
}

/// Socket inode of the LISTEN (state 0A) socket on `port`, from the text of /proc/net/tcp{,6}.
pub fn parse_listen_inode(table: &str, port: u16) -> Option<u64> {
    table.lines().skip(1).find_map(|l| {
        let f: Vec<&str> = l.split_whitespace().collect();
        if f.len() < 10 || f[3] != "0A" {
            return None;
        }
        let p = u16::from_str_radix(f[1].rsplit(':').next()?, 16).ok()?;
        if p == port {
            f[9].parse().ok()
        } else {
            None
        }
    })
}

fn all_pids() -> Vec<u32> {
    fs::read_dir("/proc").into_iter().flatten().filter_map(|e| e.ok()?.file_name().to_str()?.parse().ok()).collect()
}

fn pid_of_inode(inode: u64) -> Option<u32> {
    let want = format!("socket:[{inode}]");
    all_pids()
        .into_iter()
        .filter(|pid| {
            fs::read_dir(format!("/proc/{pid}/fd")).is_ok_and(|d| {
                d.filter_map(Result::ok).any(|f| fs::read_link(f.path()).is_ok_and(|t| t.to_str() == Some(&want)))
            })
        })
        .min()
}

/// The server process: `pidfile`, else the single process whose command line contains
/// `pid_pattern`, else whoever owns the listening socket of the backend port.
pub fn find_pid(cfg: &Config) -> Result<u32, String> {
    if let Some(f) = &cfg.pidfile {
        let text = fs::read_to_string(f).map_err(|e| format!("pidfile {}: {e}", f.display()))?;
        return text.trim().parse().map_err(|_| format!("pidfile {} does not contain a pid", f.display()));
    }
    if let Some(pat) = &cfg.pid_pattern {
        let me = std::process::id();
        let hits: Vec<u32> = all_pids()
            .into_iter()
            .filter(|&p| {
                p != me
                    && fs::read(format!("/proc/{p}/cmdline"))
                        .is_ok_and(|c| String::from_utf8_lossy(&c).replace('\0', " ").contains(pat.as_str()))
            })
            .collect();
        return match hits.as_slice() {
            [p] => Ok(*p),
            [] => Err(format!("no process matches pid_pattern {pat:?}")),
            _ => Err(format!("{} processes match pid_pattern {pat:?}", hits.len())),
        };
    }
    let port: u16 =
        cfg.backend.rsplit(':').next().and_then(|p| p.parse().ok()).ok_or("cannot read the backend port")?;
    let inode = ["/proc/net/tcp", "/proc/net/tcp6"]
        .iter()
        .find_map(|t| parse_listen_inode(&fs::read_to_string(t).ok()?, port))
        .ok_or_else(|| format!("nothing is listening on port {port}"))?;
    pid_of_inode(inode).ok_or_else(|| "cannot find the process owning the listening socket (needs root)".into())
}

/// What to signal: the whole process group when the server leads its own group, otherwise the
/// server and its descendants. Never a group the server merely shares with its parent
/// (a server panel, a wrapper shell, ...), which would freeze the parent too.
pub fn targets(pid: u32) -> Vec<i64> {
    let Some((_, _, pgid)) = proc_stat(pid) else { return vec![] };
    if pgid == pid {
        return vec![-i64::from(pid)];
    }
    let parents: Vec<(u32, u32)> = all_pids().into_iter().filter_map(|p| Some((p, proc_stat(p)?.1))).collect();
    let mut all = vec![pid];
    let mut i = 0;
    while i < all.len() {
        let cur = all[i];
        for &(p, pp) in &parents {
            if pp == cur && !all.contains(&p) {
                all.push(p);
            }
        }
        i += 1;
    }
    all.into_iter().map(i64::from).collect()
}

async fn signal(sig: &str, targets: &[i64]) -> bool {
    Command::new("sh")
        .arg("-c")
        .arg(format!("kill -s {sig} -- \"$@\""))
        .arg("sh")
        .args(targets.iter().map(i64::to_string))
        .status()
        .await
        .is_ok_and(|s| s.success())
}

fn tick_value(props: &str) -> Option<String> {
    props.lines().filter_map(|l| l.trim().strip_prefix("max-tick-time=")).next_back().map(|v| v.trim().to_string())
}

/// Why freezing must not be used right now (None = allowed). A frozen server wakes up thinking a
/// tick took minutes; with a finite `max-tick-time` the watchdog then crashes it. (Forge also has its
/// own watchdog in config/forge.cfg; check it too before using freeze with Forge.)
pub fn refusal(cfg: &Config, up_secs: u64) -> Option<String> {
    if up_secs < cfg.min_up_secs {
        return Some(format!("min_up_secs has not elapsed ({up_secs} s < {} s)", cfg.min_up_secs));
    }
    let Some(path) = &cfg.properties_file else {
        return Some("properties_file is not set, cannot verify max-tick-time=-1".into());
    };
    let text = match fs::read_to_string(path) {
        Ok(t) => t,
        Err(e) => return Some(format!("cannot read {}: {e}", path.display())),
    };
    match tick_value(&text).as_deref() {
        Some("-1") => None,
        Some(v) => {
            Some(format!("max-tick-time is {v}, must be -1 (the watchdog would crash the server after a freeze)"))
        }
        None => Some(
            "max-tick-time is not set in server.properties (default 60000 would crash the server after a freeze)"
                .into(),
        ),
    }
}

/// Pause the server. Verifies the process really reached state T; otherwise resumes it and fails.
pub async fn freeze(cfg: &Config, up_secs: u64) -> Result<Frozen, String> {
    if let Some(why) = refusal(cfg, up_secs) {
        return Err(why);
    }
    let pid = find_pid(cfg)?;
    let targets = targets(pid);
    if targets.is_empty() {
        return Err(format!("process {pid} is gone"));
    }
    if !signal("STOP", &targets).await {
        return Err("kill -s STOP failed".into());
    }
    for _ in 0..10 {
        sleep(Duration::from_millis(100)).await;
        if proc_stat(pid).map(|s| s.0) == Some('T') {
            return Ok(Frozen { pid, targets });
        }
    }
    signal("CONT", &targets).await;
    Err(format!("process {pid} did not enter the stopped state"))
}

pub async fn resume(f: &Frozen) -> bool {
    signal("CONT", &f.targets).await
}

/// After a dormant restart: is the backend process still frozen (state T) from last time?
pub fn adopt(cfg: &Config) -> Option<Frozen> {
    let pid = find_pid(cfg).ok()?;
    (proc_stat(pid)?.0 == 'T').then(|| Frozen { pid, targets: targets(pid) })
}

/// Lines for `--check`.
pub fn describe(cfg: &Config) -> Vec<String> {
    let mut out = vec![format!("sleep mode: {:?}", cfg.sleep_mode)];
    if cfg.sleep_mode != crate::SleepMode::Freeze {
        return out;
    }
    match find_pid(cfg) {
        Ok(pid) => {
            let st = proc_stat(pid).map_or('?', |s| s.0);
            out.push(format!("  would freeze pid {pid} (state {st}), signal targets {:?}", targets(pid)));
        }
        Err(e) => out.push(format!("  cannot find the process to freeze: {e}")),
    }
    out.push(match refusal(cfg, u64::MAX) {
        None => "  max-tick-time check: ok (-1), freeze is allowed".into(),
        Some(why) => format!("  freeze would be REFUSED (falls back to stop): {why}"),
    });
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg(extra: &str) -> Config {
        toml::from_str(&format!("listen=\"x\"\nbackend=\"127.0.0.1:1\"\nstart_cmd=\"true\"\nstop_cmd=\"true\"\nmotd_sleeping=\"z\"\nmin_up_secs=60\n{extra}")).unwrap()
    }

    #[test]
    fn stat_with_awkward_command_name() {
        assert_eq!(parse_stat("123 (java (x) y) T 77 123 123 0 -1 4194560"), Some(('T', 77, 123)));
        assert_eq!(parse_stat("5 (sh) S 1 5 5 0"), Some(('S', 1, 5)));
        assert_eq!(parse_stat("garbage"), None);
    }

    #[test]
    fn listen_inode_from_proc_net_tcp() {
        let t = "  sl  local_address rem_address   st tx_queue rx_queue tr tm->when retrnsmt   uid  timeout inode\n\
                 0: 0100007F:63DC 00000000:0000 0A 00000000:00000000 00:00000000 00000000     0        0 4242 1 x\n\
                 1: 0100007F:63DD 0100007F:9999 01 00000000:00000000 00:00000000 00000000     0        0 99 1 x\n";
        assert_eq!(parse_listen_inode(t, 25564), Some(4242)); // 0x63DC
        assert_eq!(parse_listen_inode(t, 25565), None); // 0x63DD is not LISTEN
    }

    fn props(name: &str, body: &str) -> String {
        let p = std::env::temp_dir().join(format!("dormant-test-{}-{name}", std::process::id()));
        fs::write(&p, body).unwrap();
        p.to_str().unwrap().to_string()
    }

    #[test]
    fn freeze_is_refused_unless_safe() {
        let ok = props("ok", "motd=x\nmax-tick-time=-1\n");
        let c = cfg(&format!("sleep_mode=\"freeze\"\nproperties_file=\"{ok}\""));
        assert_eq!(refusal(&c, 61), None);
        assert!(refusal(&c, 59).unwrap().contains("min_up_secs"));

        let finite = props("finite", "max-tick-time=60000\n");
        let c = cfg(&format!("sleep_mode=\"freeze\"\nproperties_file=\"{finite}\""));
        assert!(refusal(&c, 100).unwrap().contains("must be -1"));

        let unset = props("unset", "motd=x\n");
        let c = cfg(&format!("sleep_mode=\"freeze\"\nproperties_file=\"{unset}\""));
        assert!(refusal(&c, 100).unwrap().contains("not set"));

        assert!(refusal(&cfg("sleep_mode=\"freeze\""), 100).unwrap().contains("properties_file"));
        assert!(refusal(&cfg("properties_file=\"/nonexistent/x\""), 100).unwrap().contains("cannot read"));
    }
}
