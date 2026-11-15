//! Config loading (a single-server file or a `[[server]]` list) and the `new` / `list` subcommands.

use std::collections::HashSet;
use std::fs;
use std::path::Path;
use std::process::ExitCode;

use serde::Deserialize;

use crate::{freeze, load_boots, median, ping_backend, players_online, Config, SleepMode};

pub const DEFAULT_CONFIG: &str = "/etc/dormant/dormant.toml";
const UNKNOWN_MOTD: &str = "§cUnknown server\n§7Check the address you typed";

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Multi {
    listen: String,
    /// Name of the server that gets hostnames nobody claims (default: answer "unknown server").
    default: Option<String>,
    unknown_motd: Option<String>,
    server: Vec<Config>,
}

#[derive(Debug)]
pub struct Loaded {
    pub listen: String,
    pub default: Option<String>,
    pub unknown_motd: String,
    pub servers: Vec<Config>,
}

/// Load a config. A file without `[[server]]` is the old single-server format (a list of one).
pub fn load(path: &str) -> Result<Loaded, String> {
    let text = fs::read_to_string(path).map_err(|e| format!("cannot read {path}: {e}"))?;
    let v: toml::Value = text.parse().map_err(|e| format!("{path}: {e}"))?;
    let mut l = if v.get("server").is_some() {
        let m: Multi = v.try_into().map_err(|e| format!("{path}: {e}"))?;
        if m.server.iter().any(|s| s.listen.is_some()) {
            return Err("`listen` is a top-level key in a [[server]] file, not a per-server one".into());
        }
        Loaded { listen: m.listen, default: m.default, unknown_motd: m.unknown_motd.unwrap_or_else(|| UNKNOWN_MOTD.into()), servers: m.server }
    } else {
        let c: Config = v.try_into().map_err(|e| format!("{path}: {e}"))?;
        let listen = c.listen.clone().ok_or("`listen` is missing")?;
        Loaded { listen, default: None, unknown_motd: UNKNOWN_MOTD.into(), servers: vec![c] }
    };
    validate(&mut l)?;
    Ok(l)
}

/// Fill in names, normalise hostnames and reject anything two servers would fight over.
fn validate(l: &mut Loaded) -> Result<(), String> {
    let single = l.servers.len() == 1;
    let (mut names, mut hosts, mut files) = (HashSet::new(), HashSet::new(), HashSet::new());
    for (i, s) in l.servers.iter_mut().enumerate() {
        if s.name.is_empty() { s.name = if single { "main".into() } else { format!("server{}", i + 1) }; }
        if !names.insert(s.name.clone()) { return Err(format!("duplicate server name {:?}", s.name)); }
        for h in s.hosts.iter_mut() {
            *h = h.trim().trim_end_matches('.').to_ascii_lowercase();
            if !hosts.insert(h.clone()) { return Err(format!("host {h:?} is claimed by two servers")); }
        }
        if s.hosts.is_empty() && !single && l.default.as_deref() != Some(&s.name) {
            return Err(format!("server {:?} has no hosts and is not the default", s.name));
        }
        for f in [&s.cache_file, &s.state_file].into_iter().flatten() {
            if !files.insert(f.clone()) { return Err(format!("{} is used by two servers", f.display())); }
        }
    }
    match &l.default {
        Some(d) if !names.contains(d) => Err(format!("default server {d:?} does not exist")),
        _ => Ok(()),
    }
}

/// `dormant list [config]`: every server with its (probed) state, wake time and players. The state is
/// what the backend answers right now, so "starting" shows up as "not answering".
pub async fn cmd_list(args: &[String]) -> ExitCode {
    let path = args.first().map_or(DEFAULT_CONFIG, String::as_str);
    let l = match load(path) {
        Ok(l) => l,
        Err(e) => { eprintln!("config error: {e}"); return ExitCode::from(2); }
    };
    println!("{path}: listening on {}", l.listen);
    println!("{:<10} {:<36} {:<6} {:<22} {:<16} {}", "NAME", "HOSTS", "MODE", "STATE", "WAKE TIME", "BACKEND");
    for s in &l.servers {
        let state = if s.sleep_mode == SleepMode::Freeze && freeze::adopt(s).is_some() {
            "frozen (asleep)".to_string()
        } else {
            match ping_backend(&s.backend, s.protocol).await {
                Ok(js) => format!("up, {} players", players_online(&js).unwrap_or(0)),
                Err(_) => "asleep / not answering".into(),
            }
        };
        let boots = load_boots(s);
        let wake = match median(&boots) {
            Some(m) => format!("{m} s (last {})", boots.len()),
            None => format!("{} s (default)", s.wake_secs_default),
        };
        let mut hosts = if s.hosts.is_empty() { "(any)".into() } else { s.hosts.join(",") };
        if l.default.as_deref() == Some(&s.name) { hosts.push_str(" *"); }
        println!("{:<10} {:<36} {:<6} {:<22} {:<16} {}", s.name, hosts, format!("{:?}", s.sleep_mode).to_lowercase(), state, wake, s.backend);
    }
    if l.default.is_some() { println!("* = default server (unmatched hostnames)"); }
    ExitCode::SUCCESS
}

fn ident_ok(s: &str, extra: &str) -> bool {
    !s.is_empty() && s.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || extra.contains(c))
}

/// `s` as a TOML string literal, quoted and escaped as needed.
fn quoted(s: &str) -> String {
    toml::Value::String(s.to_string()).to_string()
}

/// What `dormant new` was asked for.
struct NewServer {
    name: String,
    port: u16,
    hosts: Vec<String>,
    start_cmd: String,
    stop_cmd: String,
    mode: String,
    properties_file: Option<String>,
}

/// The `[[server]]` block that `dormant new` appends.
fn server_block(n: &NewServer) -> String {
    let list = n.hosts.iter().map(|h| quoted(h)).collect::<Vec<_>>().join(", ");
    let freeze = match &n.properties_file {
        Some(f) => format!("properties_file = {}   # freeze is refused unless it has max-tick-time=-1\n", quoted(f)),
        None => String::new(),
    };
    r#"[[server]]
name = "@NAME@"
hosts = [@HOSTS@]
backend = "127.0.0.1:@PORT@"
start_cmd = @START@
stop_cmd  = @STOP@
sleep_mode = "@MODE@"
@FREEZE@idle_secs = 900
min_up_secs = 600
wake_secs_default = 90
# wake_names = ["Player1"]    # only these may wake it; empty = anyone
# version_name = "1.21.1"     # shown until a real status has been cached
# protocol = 767
motd_sleeping = "§6@NAME@\n§7Asleep - join to wake it (~{wake_secs} s)"
motd_starting = "§2Waking up...\n§7Join again in ~{wake_secs} s"
kick_starting = "§6@NAME@ is waking up!§r\n\nWaking takes about {wake_secs} seconds.\nTry again in about {secs} seconds."
cache_file = "/var/lib/dormant/@NAME@-status.json"
"#
    // The free-text values go in last so that nothing inside them is taken for a placeholder.
    .replace("@NAME@", &n.name).replace("@HOSTS@", &list).replace("@PORT@", &n.port.to_string())
    .replace("@MODE@", &n.mode).replace("@FREEZE@", &freeze)
    .replace("@STOP@", &quoted(&n.stop_cmd)).replace("@START@", &quoted(&n.start_cmd))
}

const NEW_USAGE: &str = "usage: dormant new <name> --backend-port <n> --hosts a.example.com,b.example.com
                   --start-cmd <command> --stop-cmd <command>
                   [--mode stop|freeze] [--properties-file <server.properties>] [--config <path>]

Appends a [[server]] block to the config (default /etc/dormant/dormant.toml) or creates the file.
The result is validated before anything is written; an existing file is backed up to <config>.bak.

  --start-cmd / --stop-cmd   shell commands (run with sh -c) that start / stop the real server and
                             return quickly, e.g. \"systemctl start minecraft\"
  --crafty-id <id>           shortcut for a server managed by Crafty Controller: uses
                             \"/usr/local/bin/crafty-ctl <id> start|stop\" (see examples/crafty)
  --mode freeze              pause the server process instead of stopping it; needs --properties-file";

/// `dormant new`: see NEW_USAGE.
pub fn cmd_new(args: &[String]) -> ExitCode {
    if args.is_empty() || args.iter().any(|a| a == "--help" || a == "-h") {
        println!("{NEW_USAGE}");
        return ExitCode::from(if args.is_empty() { 2 } else { 0 });
    }
    match new_inner(args) {
        Ok(msg) => { println!("{msg}"); ExitCode::SUCCESS }
        Err(e) => { eprintln!("dormant new: {e}"); ExitCode::from(2) }
    }
}

fn new_inner(args: &[String]) -> Result<String, String> {
    let mut it = args.iter();
    let name = it.next().filter(|n| !n.starts_with("--")).ok_or(NEW_USAGE)?.clone();
    let (mut crafty, mut port, mut hosts, mut mode) = (None, None, None, "stop".to_string());
    let (mut start_cmd, mut stop_cmd, mut properties_file) = (None, None, None);
    let mut path = DEFAULT_CONFIG.to_string();
    while let Some(flag) = it.next() {
        let val = it.next().ok_or(NEW_USAGE)?.clone();
        match flag.as_str() {
            "--start-cmd" => start_cmd = Some(val),
            "--stop-cmd" => stop_cmd = Some(val),
            "--crafty-id" => crafty = Some(val),
            "--backend-port" => port = Some(val.parse::<u16>().map_err(|_| "bad --backend-port")?),
            "--hosts" => hosts = Some(val.split(',').map(|h| h.trim().trim_end_matches('.').to_ascii_lowercase()).collect::<Vec<_>>()),
            "--mode" => mode = val,
            "--properties-file" => properties_file = Some(val),
            "--config" => path = val,
            _ => return Err(format!("unknown option {flag}\n{NEW_USAGE}")),
        }
    }
    let (port, hosts) = (port.ok_or("--backend-port is missing")?, hosts.ok_or("--hosts is missing")?);
    if !ident_ok(&name, "-_") { return Err("name may only contain a-z, 0-9, - and _".into()); }
    if mode != "stop" && mode != "freeze" { return Err("--mode must be stop or freeze".into()); }
    if hosts.iter().any(|h| !ident_ok(h, "-.")) { return Err("--hosts must be plain hostnames like mc.example.com".into()); }
    if mode == "freeze" && properties_file.is_none() {
        return Err("--mode freeze needs --properties-file <path to the server's server.properties>".into());
    }
    let (start_cmd, stop_cmd) = match (start_cmd, stop_cmd, crafty) {
        (Some(a), Some(b), None) => (a, b),
        (None, None, Some(id)) => {
            if !ident_ok(&id, "-") { return Err("--crafty-id must be the server's id from Crafty (lowercase hex and dashes)".into()); }
            (format!("/usr/local/bin/crafty-ctl {id} start"), format!("/usr/local/bin/crafty-ctl {id} stop"))
        }
        _ => return Err("give both --start-cmd and --stop-cmd (or --crafty-id instead of the two)".into()),
    };
    if start_cmd.contains('\n') || stop_cmd.contains('\n') { return Err("commands must be a single line".into()); }

    let block = server_block(&NewServer { name: name.clone(), port, hosts: hosts.clone(), start_cmd, stop_cmd, mode, properties_file });
    let existing = Path::new(&path).exists();
    let text = if existing {
        let old = fs::read_to_string(&path).map_err(|e| format!("cannot read {path}: {e}"))?;
        let v: toml::Value = old.parse().map_err(|e| format!("{path}: {e}"))?;
        if v.get("server").is_none() {
            return Err(format!("{path} is a single-server file; turn it into a [[server]] file first (see the README, 'Several servers on one port')"));
        }
        format!("{}\n\n{block}", old.trim_end())
    } else {
        format!("# dormant: one listener, several servers chosen by the hostname the player typed.\nlisten = \"0.0.0.0:25565\"\n# default = \"{name}\"    # server for hostnames nobody claims; without it they get an 'unknown server' reply\n\n{block}")
    };
    let tmp = format!("{path}.new");
    if let Some(dir) = Path::new(&path).parent() { fs::create_dir_all(dir).ok(); }
    fs::write(&tmp, &text).map_err(|e| format!("cannot write {tmp}: {e}"))?;
    if let Err(e) = load(&tmp) { fs::remove_file(&tmp).ok(); return Err(format!("not written, the result would be invalid: {e}")); }
    if existing { fs::copy(&path, format!("{path}.bak")).map_err(|e| e.to_string())?; }
    fs::rename(&tmp, &path).map_err(|e| e.to_string())?;
    Ok(format!("{} server {name:?} ({}) in {path}{}\n\nStill to do by hand:\n  1. DNS: point {} at the machine dormant listens on (dormant never touches DNS).\n  2. Review the new block in {path} (MOTD texts, wake_names, version_name / protocol).\n  3. Pick up the new server: restart dormant (this drops connected players: do it when nobody is online).\n  4. Check: `dormant --check {path}` and `dormant list {path}`.",
        if existing { "Added" } else { "Created" }, hosts.join(", "), if existing { format!(" (backup: {path}.bak)") } else { String::new() }, hosts.join(", ")))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(name: &str, body: &str) -> String {
        let p = std::env::temp_dir().join(format!("dormant-cli-{}-{name}", std::process::id()));
        fs::write(&p, body).unwrap();
        p.to_str().unwrap().to_string()
    }

    const SRV: &str = "backend=\"127.0.0.1:1\"\nstart_cmd=\"true\"\nstop_cmd=\"true\"\nmotd_sleeping=\"z\"\n";

    #[test]
    fn old_single_server_file_still_loads() {
        let l = load(&write("single.toml", &format!("listen=\"0.0.0.0:25565\"\n{SRV}"))).unwrap();
        assert_eq!((l.servers.len(), l.servers[0].name.as_str(), l.listen.as_str()), (1, "main", "0.0.0.0:25565"));
    }

    #[test]
    fn server_list_is_validated() {
        let ok = format!("listen=\"x\"\ndefault=\"a\"\n[[server]]\nname=\"a\"\nhosts=[\"A.Example.\"]\n{SRV}[[server]]\nname=\"b\"\nhosts=[\"b.example\"]\n{SRV}");
        let l = load(&write("ok.toml", &ok)).unwrap();
        assert_eq!(l.servers[0].hosts, vec!["a.example"]); // lowercased, trailing dot removed
        let dup = ok.replace("b.example", "a.example");
        assert!(load(&write("dup.toml", &dup)).unwrap_err().contains("claimed by two"));
        let nodef = ok.replace("default=\"a\"\n", "default=\"zzz\"\n");
        assert!(load(&write("nodef.toml", &nodef)).unwrap_err().contains("does not exist"));
        let nohost = ok.replace("hosts=[\"b.example\"]\n", "");
        assert!(load(&write("nohost.toml", &nohost)).unwrap_err().contains("no hosts"));
        let inner = ok.replace("name=\"b\"\n", "name=\"b\"\nlisten=\"y\"\n");
        assert!(load(&write("inner.toml", &inner)).unwrap_err().contains("top-level"));
    }

    #[test]
    fn new_creates_then_appends_and_rejects_duplicates() {
        let path = write("new.toml", "");
        fs::remove_file(&path).unwrap();
        let args = |n: &str, h: &str, extra: &[&str]| -> Vec<String> {
            [n, "--backend-port", "25570", "--hosts", h, "--config", &path].iter().chain(extra).map(|s| s.to_string()).collect()
        };
        let cmds = ["--start-cmd", "systemctl start \"mc one\"", "--stop-cmd", "systemctl stop mc"];
        let out = new_inner(&args("modded", "modded.example.com", &cmds)).unwrap();
        assert!(out.contains("Created") && out.contains("DNS"));
        new_inner(&args("vanilla", "vanilla.example.com,mc.example.com", &["--crafty-id", "0a1b2c3d-0000-4000-8000-123456789abc"])).unwrap();
        let l = load(&path).unwrap();
        assert_eq!(l.servers.iter().map(|s| s.name.as_str()).collect::<Vec<_>>(), ["modded", "vanilla"]);
        assert_eq!(l.servers[0].start_cmd, "systemctl start \"mc one\""); // quotes survive the TOML round trip
        assert_eq!(l.servers[0].stop_cmd, "systemctl stop mc");
        assert!(l.servers[1].start_cmd.ends_with("crafty-ctl 0a1b2c3d-0000-4000-8000-123456789abc start"));
        assert!(new_inner(&args("modded", "other.example.com", &cmds)).unwrap_err().contains("duplicate"));
        assert!(new_inner(&args("x", "mc.example.com", &cmds)).unwrap_err().contains("claimed"));
        assert_eq!(load(&path).unwrap().servers.len(), 2); // failed adds left the file alone
        fs::remove_file(&path).ok();
        fs::remove_file(format!("{path}.bak")).ok();
    }

    #[test]
    fn new_needs_commands_and_a_properties_file_for_freeze() {
        let path = write("new2.toml", "");
        fs::remove_file(&path).unwrap();
        let args = |extra: &[&str]| -> Vec<String> {
            ["a", "--backend-port", "25570", "--hosts", "a.example.com", "--config", &path].iter().chain(extra).map(|s| s.to_string()).collect()
        };
        assert!(new_inner(&args(&[])).unwrap_err().contains("--start-cmd"));
        assert!(new_inner(&args(&["--start-cmd", "x"])).unwrap_err().contains("--stop-cmd"));
        assert!(new_inner(&args(&["--start-cmd", "x", "--stop-cmd", "y", "--crafty-id", "abc"])).unwrap_err().contains("--crafty-id"));
        assert!(new_inner(&args(&["--start-cmd", "x", "--stop-cmd", "y", "--mode", "freeze"])).unwrap_err().contains("--properties-file"));
        assert!(!Path::new(&path).exists());
        new_inner(&args(&["--start-cmd", "x", "--stop-cmd", "y", "--mode", "freeze", "--properties-file", "/srv/mc/server.properties"])).unwrap();
        let l = load(&path).unwrap();
        assert_eq!(l.servers[0].sleep_mode, SleepMode::Freeze);
        assert_eq!(l.servers[0].properties_file.as_deref(), Some(Path::new("/srv/mc/server.properties")));
        fs::remove_file(&path).ok();
    }
}
