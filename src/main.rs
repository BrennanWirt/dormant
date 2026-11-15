//! dormant: a small Minecraft "sleep proxy" for any Java server (vanilla, Forge, NeoForge, Fabric).
//!
//! It owns the public port. While the real server is off it answers server-list pings itself, using
//! the last status reply it saw from the real server (kept as an opaque string; only `description`
//! and `players` are edited). A join attempt by an allowed player starts the server and is kicked
//! with a friendly "try again in N s". While the server is up, connections are forwarded untouched
//! (the handshake, including Forge's "\0FML\0" address suffix, is replayed byte for byte). The
//! backend's own status ping decides when the server has been empty long enough to stop.

use std::io;
use std::net::IpAddr;
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde::Deserialize;
use serde_json::{json, Value};
use tokio::io::{copy_bidirectional, AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::process::Command;
use tokio::sync::Semaphore;
use tokio::time::{sleep, timeout};

mod cli;
mod freeze;
mod limit;
use freeze::Frozen;
use limit::{Denied, WakeLimiter};

/// Sanity limit for the backend's status packet (the protocol allows up to 2 MiB).
const MAX_BACKEND_PACKET: i32 = 16 << 20;
/// Limit for packets from (untrusted) clients: handshake, login start, status request, ping.
const MAX_CLIENT_PACKET: i32 = 4096;
const MAX_CONNECTIONS: usize = 512;

/// Structured log line: `level=info event=wake player=Steve`.
macro_rules! log {
    ($lvl:expr, $ev:expr $(, $k:ident = $v:expr)* $(,)?) => {{
        #[allow(unused_mut)]
        let mut line = format!("level={} event={}", $lvl, $ev);
        $( line.push_str(&format!(concat!(" ", stringify!($k), "={}"), quote(&$v.to_string()))); )*
        eprintln!("{line}");
    }};
}

/// `log!` with the server's name added (`$sh` is anything with a `.cfg`).
macro_rules! slog {
    ($sh:expr, $lvl:expr, $ev:expr $(, $k:ident = $v:expr)* $(,)?) => {
        log!($lvl, $ev, server = $sh.cfg.name $(, $k = $v)*)
    };
}

fn quote(s: &str) -> String {
    if s.is_empty() || s.contains(|c: char| c.is_whitespace() || c == '"' || c == '=') {
        format!("{s:?}")
    } else {
        s.to_string()
    }
}

#[derive(Deserialize, Clone, Debug)]
#[serde(deny_unknown_fields)]
struct Config {
    /// Name used in logs and `dormant list` (default "main", or "serverN" in a [[server]] file).
    #[serde(default)]
    name: String,
    /// Hostnames players type for this server (case-insensitive; Forge's \0FML\0 suffix and a trailing
    /// dot are ignored). Empty is allowed for a single server or for the `default` server.
    #[serde(default)]
    hosts: Vec<String>,
    /// Public address to listen on, e.g. "0.0.0.0:25565". Single-server files only; in a
    /// [[server]] file `listen` is a top-level key.
    listen: Option<String>,
    /// The real server, e.g. "127.0.0.1:25572".
    backend: String,
    /// Shell commands (run with `sh -c`) that start / stop the real server and return quickly.
    start_cmd: String,
    stop_cmd: String,
    /// MOTDs and kick texts may contain `{wake_secs}` (median of the last 5 measured wake times, or
    /// `wake_secs_default` until one has been measured) and, in kick texts, `{secs}` (time left).
    motd_sleeping: String,
    motd_starting: Option<String>,
    motd_stopping: Option<String>,
    kick_starting: Option<String>,
    kick_stopping: Option<String>,
    kick_denied: Option<String>,
    #[serde(default = "d900")]
    idle_secs: u64,
    #[serde(default = "d600")]
    min_up_secs: u64,
    #[serde(default = "d90", alias = "boot_estimate_secs")]
    wake_secs_default: u64,
    /// "stop" (default): run stop_cmd when idle. "freeze": SIGSTOP the server process instead and
    /// resume it on a login (no kick, the joiner is forwarded). See freeze.rs for the safety checks.
    #[serde(default)]
    sleep_mode: SleepMode,
    /// Freeze mode: which process to pause. Default: the owner of the backend port.
    pidfile: Option<PathBuf>,
    pid_pattern: Option<String>,
    /// Freeze mode: the server's server.properties; freezing is refused unless max-tick-time=-1.
    properties_file: Option<PathBuf>,
    /// Measured wake times; default: next to cache_file.
    state_file: Option<PathBuf>,
    #[serde(default = "d600")]
    wake_timeout_secs: u64,
    #[serde(default = "d5")]
    poll_secs: u64,
    /// Max players shown while asleep (default: whatever the last real reply said, else 20).
    max_players: Option<u64>,
    /// Used for the sleeping reply only when no real reply has ever been cached.
    #[serde(default = "d_version")]
    version_name: String,
    #[serde(default = "d340")]
    protocol: i32,
    /// If non-empty, only these names (case-insensitive) may wake the server. The name is whatever the
    /// client sent (dormant cannot authenticate it), so this keeps strangers out but is not a secret.
    #[serde(default)]
    wake_names: Vec<String>,
    /// Wake rate limits: at most this many wakes (start, resume or queued start) per source address
    /// (IPv4 address or IPv6 /64) and for this server in total, within `wake_limit_window_secs`.
    /// 0 switches a limit off.
    #[serde(default = "d3")]
    wake_limit_per_ip: usize,
    #[serde(default = "d6")]
    wake_limit_global: usize,
    #[serde(default = "d600")]
    wake_limit_window_secs: u64,
    kick_rate_limited: Option<String>,
    /// Where to keep the last real status reply across restarts.
    cache_file: Option<PathBuf>,
}

fn d900() -> u64 {
    900
}
fn d90() -> u64 {
    90
}
fn d600() -> u64 {
    600
}
fn d5() -> u64 {
    5
}
fn d3() -> usize {
    3
}
fn d6() -> usize {
    6
}
fn d340() -> i32 {
    340
}
fn d_version() -> String {
    "1.12.2".into()
}

#[derive(Deserialize, Default, Clone, Copy, PartialEq, Debug)]
#[serde(rename_all = "lowercase")]
enum SleepMode {
    #[default]
    Stop,
    Freeze,
}

#[derive(Clone, Copy, PartialEq, Debug)]
enum Phase {
    Sleeping,
    Starting,
    Up,
    Stopping,
    Frozen,
}

struct State {
    phase: Phase,
    since: Instant,
    last_active: Instant,
    /// Last good status reply from the backend, verbatim.
    cache: Option<String>,
    cache_saved: Option<Instant>,
    /// The last 5 measured wake times in seconds (start command / resume to first full status).
    boots: Vec<u64>,
    wake_pending: bool,
    frozen: Option<Frozen>,
}

/// Median of a list (mean of the two middle values when the count is even).
fn median(v: &[u64]) -> Option<u64> {
    let mut s = v.to_vec();
    s.sort_unstable();
    let n = s.len();
    (n > 0).then(|| if n % 2 == 1 { s[n / 2] } else { (s[n / 2 - 1] + s[n / 2]) / 2 })
}

fn fill(text: &str, wake_secs: u64, left_secs: u64) -> String {
    text.replace("{wake_secs}", &wake_secs.to_string()).replace("{secs}", &left_secs.to_string())
}

/// The saved wake times of a server (empty if none measured yet).
fn load_boots(cfg: &Config) -> Vec<u64> {
    Shared::state_path(cfg)
        .and_then(|p| std::fs::read_to_string(p).ok())
        .and_then(|t| serde_json::from_str::<Value>(&t).ok())
        .and_then(|v| v["boots"].as_array().map(|a| a.iter().filter_map(Value::as_u64).collect()))
        .unwrap_or_default()
}

enum Join {
    Kick(String),
    Resume,
}

/// Routes a handshake's server address to one of the configured servers.
struct Router {
    servers: Vec<Arc<Shared>>,
    default: Option<usize>,
    unknown_motd: String,
}

/// The hostname the player typed: cut at the first NUL (Forge appends "\0FML\0", "\0FORGE\0", ...),
/// drop a trailing dot, lowercase.
fn host_of(addr: &[u8]) -> String {
    let s = String::from_utf8_lossy(addr);
    s.split('\0').next().unwrap_or("").trim_end_matches('.').to_ascii_lowercase()
}

impl Router {
    fn route(&self, host: &str) -> Option<&Arc<Shared>> {
        self.servers
            .iter()
            .find(|s| s.cfg.hosts.iter().any(|h| h == host))
            .or_else(|| self.default.map(|i| &self.servers[i]))
    }
}

struct Shared {
    cfg: Config,
    st: Mutex<State>,
    conns: AtomicUsize,
    limiter: WakeLimiter,
}

fn set_phase(st: &mut State, p: Phase) {
    st.phase = p;
    st.since = Instant::now();
}

impl Shared {
    fn new(cfg: Config) -> Arc<Self> {
        let cache = cfg.cache_file.as_ref().and_then(|p| std::fs::read_to_string(p).ok());
        let boots = load_boots(&cfg);
        // A frozen backend left over from a previous dormant run counts as asleep, not as "not answering".
        let frozen = if cfg.sleep_mode == SleepMode::Freeze { freeze::adopt(&cfg) } else { None };
        if let Some(f) = &frozen {
            log!("info", "adopted_frozen", server = cfg.name, pid = f.pid);
        }
        let now = Instant::now();
        Arc::new(Shared {
            conns: AtomicUsize::new(0),
            limiter: WakeLimiter::new(cfg.wake_limit_per_ip, cfg.wake_limit_global, cfg.wake_limit_window_secs),
            st: Mutex::new(State {
                phase: if frozen.is_some() { Phase::Frozen } else { Phase::Sleeping },
                since: now,
                last_active: now,
                cache,
                cache_saved: None,
                boots,
                wake_pending: false,
                frozen,
            }),
            cfg,
        })
    }

    fn state_path(cfg: &Config) -> Option<PathBuf> {
        cfg.state_file.clone().or_else(|| cfg.cache_file.as_ref().map(|p| p.with_extension("state.json")))
    }

    fn phase(&self) -> Phase {
        self.st.lock().unwrap().phase
    }

    fn wake_secs(&self, st: &State) -> u64 {
        median(&st.boots).unwrap_or(self.cfg.wake_secs_default)
    }

    /// The backend answered with a full status: record how long the wake took and switch to Up.
    fn became_up(&self, st: &mut State, online: Option<u64>) {
        let from = st.phase;
        if from == Phase::Starting {
            st.boots.push(st.since.elapsed().as_secs_f64().ceil() as u64); // whole seconds, at least 1
            if st.boots.len() > 5 {
                st.boots.remove(0);
            }
            if let Some(p) = Self::state_path(&self.cfg) {
                std::fs::write(p, json!({"boots": st.boots}).to_string()).ok(); // tiny file, written once per wake
            }
        }
        let measured = if from == Phase::Starting { st.boots.last().copied().unwrap_or(0) } else { 0 };
        slog!(
            self,
            "info",
            "up",
            from = format!("{from:?}"),
            wake_secs = measured,
            median_wake_secs = self.wake_secs(st),
            players = online.unwrap_or(0)
        );
        set_phase(st, Phase::Up);
        st.last_active = Instant::now();
        st.wake_pending = false;
        st.frozen = None;
    }

    /// Monitor tick while frozen: is the process still stopped?
    fn check_frozen(&self) {
        let mut st = self.st.lock().unwrap();
        match st.frozen.as_ref().map(|f| freeze::proc_stat(f.pid).map(|s| s.0)) {
            Some(Some('T')) => {}
            Some(Some(_)) => {
                slog!(self, "warn", "unfrozen", note = "the backend was resumed by something else");
                st.frozen = None;
                set_phase(&mut st, Phase::Starting);
            }
            _ => {
                slog!(self, "warn", "frozen_process_gone");
                st.frozen = None;
                set_phase(&mut st, Phase::Sleeping);
            }
        }
    }

    /// Freeze mode: a login arrived while frozen. Resume the process and wait up to 3 s for it to
    /// answer a status ping, so the caller can forward the joiner instead of kicking it.
    async fn resume(self: &Arc<Self>) -> bool {
        let frozen = self.st.lock().unwrap().frozen.take();
        if let Some(f) = &frozen {
            let ok = freeze::resume(f).await;
            slog!(self, if ok { "info" } else { "error" }, "resume", pid = f.pid, ok = ok);
        }
        let t0 = Instant::now();
        while t0.elapsed() < Duration::from_secs(3) {
            if let Ok(Ok(js)) =
                timeout(Duration::from_millis(500), ping_backend(&self.cfg.backend, self.cfg.protocol)).await
            {
                let mut st = self.st.lock().unwrap();
                let online = players_online(&js);
                self.became_up(&mut st, online);
                st.cache = Some(js);
                return true;
            }
            sleep(Duration::from_millis(100)).await;
        }
        false
    }

    /// Server-list reply while the backend is not up: the cached real reply with description and
    /// players replaced. Anything else in it (modinfo, forgeData, favicon, version) stays as is.
    fn sleeping_status(&self) -> String {
        let (phase, cache, wake) = {
            let st = self.st.lock().unwrap();
            (st.phase, st.cache.clone(), self.wake_secs(&st))
        };
        let motd = match phase {
            Phase::Starting => self.cfg.motd_starting.as_deref(),
            Phase::Stopping => self.cfg.motd_stopping.as_deref(),
            _ => None,
        }
        .unwrap_or(&self.cfg.motd_sleeping);
        let motd = fill(motd, wake, wake);
        let mut v = cache
            .and_then(|c| serde_json::from_str::<Value>(&c).ok())
            .filter(Value::is_object)
            .unwrap_or_else(|| json!({}));
        let o = v.as_object_mut().unwrap();
        o.entry("version").or_insert_with(|| json!({"name": self.cfg.version_name, "protocol": self.cfg.protocol}));
        o.insert("description".into(), json!({"text": motd}));
        let p = o.entry("players").or_insert_with(|| json!({}));
        if !p.is_object() {
            *p = json!({});
        }
        let p = p.as_object_mut().unwrap();
        let max = self.cfg.max_players.or_else(|| p.get("max").and_then(Value::as_u64)).unwrap_or(20);
        p.insert("max".into(), json!(max));
        p.insert("online".into(), json!(0));
        p.remove("sample");
        v.to_string()
    }

    fn kick_text(&self, which: Phase, wake: u64, left: u64) -> String {
        let t = match which {
            Phase::Stopping => self
                .cfg
                .kick_stopping
                .as_deref()
                .unwrap_or("§7The server is going to sleep.§r\n\nTry again in about a minute and it will wake up."),
            _ => self
                .cfg
                .kick_starting
                .as_deref()
                .unwrap_or("§6The server is starting!§r\n\nTry again in about {secs} seconds."),
        };
        fill(t, wake, left)
    }

    /// A login attempt while the backend is not up: a kick message, or (freeze mode) permission to
    /// resume the frozen server and forward the joiner.
    fn on_join(self: &Arc<Self>, name: &str, ip: IpAddr) -> Join {
        let w = &self.cfg.wake_names;
        if !w.is_empty() && !w.iter().any(|n| n.eq_ignore_ascii_case(name)) {
            slog!(self, "info", "wake_denied", player = name);
            return Join::Kick(
                self.cfg
                    .kick_denied
                    .clone()
                    .unwrap_or_else(|| "§cThe server is asleep and you are not allowed to wake it.".into()),
            );
        }
        let mut st = self.st.lock().unwrap();
        let est = self.wake_secs(&st);
        // A login in these phases starts, resumes or queues a start: that is what the wake limits count.
        let wakes = match st.phase {
            Phase::Sleeping | Phase::Frozen => true,
            Phase::Stopping => !st.wake_pending,
            Phase::Starting | Phase::Up => false,
        };
        let refused = if wakes { self.limiter.check(ip, Instant::now()).err() } else { None };
        if let Some(why) = refused {
            let limit = if why == Denied::Source { "per_ip" } else { "global" };
            slog!(self, "warn", "wake_rate_limited", player = name, source = ip, limit = limit);
            return Join::Kick(
                self.cfg
                    .kick_rate_limited
                    .clone()
                    .unwrap_or_else(|| "§cToo many wake-ups in a short time.§r\n\nTry again in a few minutes.".into()),
            );
        }
        match st.phase {
            Phase::Sleeping => {
                set_phase(&mut st, Phase::Starting);
                drop(st);
                slog!(self, "info", "wake", player = name);
                self.run("start", self.cfg.start_cmd.clone());
                Join::Kick(self.kick_text(Phase::Starting, est, est))
            }
            Phase::Frozen => {
                set_phase(&mut st, Phase::Starting);
                slog!(self, "info", "wake", player = name, mode = "freeze");
                Join::Resume
            }
            Phase::Starting => {
                let left = est.saturating_sub(st.since.elapsed().as_secs()).max(10);
                Join::Kick(self.kick_text(Phase::Starting, est, left))
            }
            Phase::Stopping => {
                st.wake_pending = true;
                slog!(self, "info", "wake_queued", player = name);
                Join::Kick(self.kick_text(Phase::Stopping, est, 0))
            }
            Phase::Up => Join::Kick(self.kick_text(Phase::Starting, est, 5)),
        }
    }

    /// Run a start/stop command in the background.
    fn run(self: &Arc<Self>, what: &'static str, cmd: String) {
        let sh = self.clone();
        tokio::spawn(async move {
            let r =
                timeout(Duration::from_secs(180), Command::new("sh").arg("-c").arg(&cmd).kill_on_drop(true).status())
                    .await;
            let ok = matches!(&r, Ok(Ok(s)) if s.success());
            slog!(sh, if ok { "info" } else { "error" }, "command", which = what, ok = ok, result = format!("{r:?}"));
            if !ok && what == "start" {
                let mut st = sh.st.lock().unwrap();
                if st.phase == Phase::Starting {
                    set_phase(&mut st, Phase::Sleeping);
                }
            }
        });
    }
}

// ---- protocol helpers --------------------------------------------------------------------------

fn bad(msg: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, msg.to_string())
}

fn put_varint(out: &mut Vec<u8>, v: i32) {
    let mut v = v as u32;
    loop {
        let b = (v & 0x7f) as u8;
        v >>= 7;
        if v == 0 {
            out.push(b);
            return;
        }
        out.push(b | 0x80);
    }
}

fn put_packet(out: &mut Vec<u8>, body: &[u8]) {
    put_varint(out, body.len() as i32);
    out.extend_from_slice(body);
}

fn get_varint(b: &mut &[u8]) -> io::Result<i32> {
    let mut v = 0u32;
    for i in 0..5 {
        let (&x, rest) = b.split_first().ok_or_else(|| bad("short packet"))?;
        *b = rest;
        v |= u32::from(x & 0x7f) << (7 * i);
        if x & 0x80 == 0 {
            return Ok(v as i32);
        }
    }
    Err(bad("varint too long"))
}

fn get_bytes<'a>(b: &mut &'a [u8]) -> io::Result<&'a [u8]> {
    let n = get_varint(b)?;
    if n < 0 || n as usize > b.len() {
        return Err(bad("bad string length"));
    }
    let (s, rest) = b.split_at(n as usize);
    *b = rest;
    Ok(s)
}

async fn read_varint<R: AsyncRead + Unpin>(r: &mut R) -> io::Result<i32> {
    let mut v = 0u32;
    for i in 0..5 {
        let x = r.read_u8().await?;
        v |= u32::from(x & 0x7f) << (7 * i);
        if x & 0x80 == 0 {
            return Ok(v as i32);
        }
    }
    Err(bad("varint too long"))
}

/// Read one uncompressed packet and return its body (packet id + data). Reads exactly one packet.
async fn read_packet<R: AsyncRead + Unpin>(r: &mut R, max: i32) -> io::Result<Vec<u8>> {
    let len = read_varint(r).await?;
    if len <= 0 || len > max {
        return Err(bad("bad packet length"));
    }
    let mut body = vec![0u8; len as usize];
    r.read_exact(&mut body).await?;
    Ok(body)
}

async fn send<W: AsyncWrite + Unpin>(w: &mut W, body: &[u8]) -> io::Result<()> {
    let mut out = Vec::with_capacity(body.len() + 5);
    put_packet(&mut out, body);
    w.write_all(&out).await
}

/// Packet id 0x00 followed by one string (status response and login disconnect share this shape).
fn string_packet(s: &str) -> Vec<u8> {
    let mut body = vec![0u8];
    put_varint(&mut body, s.len() as i32);
    body.extend_from_slice(s.as_bytes());
    body
}

async fn within<T>(f: impl std::future::Future<Output = io::Result<T>>) -> io::Result<T> {
    timeout(Duration::from_secs(15), f).await.unwrap_or_else(|_| Err(io::ErrorKind::TimedOut.into()))
}

// ---- backend status ping -----------------------------------------------------------------------

/// Ask the real server for its status; returns the reply JSON untouched.
async fn ping_backend(addr: &str, protocol: i32) -> io::Result<String> {
    within(async {
        let mut s = TcpStream::connect(addr).await?;
        let mut hs = Vec::new();
        put_varint(&mut hs, 0);
        put_varint(&mut hs, protocol);
        put_varint(&mut hs, 7);
        hs.extend_from_slice(b"dormant");
        hs.extend_from_slice(&0u16.to_be_bytes());
        put_varint(&mut hs, 1);
        let mut out = Vec::new();
        put_packet(&mut out, &hs);
        put_packet(&mut out, &[0]);
        s.write_all(&out).await?;
        let body = read_packet(&mut s, MAX_BACKEND_PACKET).await?;
        let mut b = &body[..];
        if get_varint(&mut b)? != 0 {
            return Err(bad("unexpected packet id"));
        }
        let js = String::from_utf8(get_bytes(&mut b)?.to_vec()).map_err(|_| bad("status is not utf-8"))?;
        // Forge 1.12.2 answers with a stub (no players) well before the server accepts logins.
        if players_online(&js).is_none() {
            return Err(bad("incomplete status (no players.online): still starting"));
        }
        Ok(js)
    })
    .await
}

fn players_online(status: &str) -> Option<u64> {
    serde_json::from_str::<Value>(status).ok()?.pointer("/players/online")?.as_u64()
}

// ---- client connections ------------------------------------------------------------------------

struct ConnGuard(Arc<Shared>);
impl Drop for ConnGuard {
    fn drop(&mut self) {
        self.0.conns.fetch_sub(1, Ordering::Relaxed);
    }
}

async fn handle(rt: Arc<Router>, mut c: TcpStream) -> io::Result<()> {
    let ip = c.peer_addr()?.ip();
    c.set_nodelay(true).ok();
    let hs = within(read_packet(&mut c, MAX_CLIENT_PACKET)).await?;
    let mut b = &hs[..];
    if get_varint(&mut b)? != 0 {
        return Err(bad("expected handshake"));
    }
    let protocol = get_varint(&mut b)?;
    // The address as typed is used only for routing; the handshake itself (with Forge's \0FML\0 suffix) is replayed as is.
    let host = host_of(get_bytes(&mut b)?);
    if b.len() < 2 {
        return Err(bad("short handshake"));
    }
    let next = get_varint(&mut &b[2..])?;

    let Some(sh) = rt.route(&host).cloned() else {
        log!("info", "unknown_host", host = host);
        return match next {
            1 => {
                let status =
                    json!({"version": {"name": "dormant", "protocol": protocol}, "players": {"max": 0, "online": 0},
                    "description": {"text": rt.unknown_motd}})
                    .to_string();
                status_loop(&mut c, || status.clone()).await
            }
            2 | 3 => {
                within(read_packet(&mut c, MAX_CLIENT_PACKET)).await?;
                send_kick(&mut c, "§cUnknown server.§r\n\nCheck the address you typed.").await
            }
            _ => Ok(()),
        };
    };
    if sh.phase() == Phase::Up {
        match forward(&sh, &mut c, &[&hs[..]]).await {
            Ok(()) => return Ok(()),
            Err(e) => slog!(sh, "warn", "backend_connect_failed", error = e),
        }
    }
    match next {
        1 => status_loop(&mut c, || sh.sleeping_status()).await,
        2 | 3 => login(&sh, &mut c, &hs, ip).await,
        _ => Ok(()),
    }
}

/// Connect to the backend, replay the packets already read from the client, then pipe both ways.
async fn forward(sh: &Arc<Shared>, c: &mut TcpStream, replay: &[&[u8]]) -> io::Result<()> {
    let mut up = TcpStream::connect(&sh.cfg.backend).await?;
    up.set_nodelay(true).ok();
    sh.conns.fetch_add(1, Ordering::Relaxed);
    let _guard = ConnGuard(sh.clone());
    for p in replay {
        send(&mut up, p).await?;
    }
    copy_bidirectional(c, &mut up).await.ok();
    Ok(())
}

async fn status_loop(c: &mut TcpStream, status: impl Fn() -> String) -> io::Result<()> {
    loop {
        let p = within(read_packet(c, MAX_CLIENT_PACKET)).await?;
        let mut b = &p[..];
        match get_varint(&mut b)? {
            0 => send(c, &string_packet(&status())).await?,
            1 => {
                let mut pong = vec![1u8];
                pong.extend_from_slice(b);
                return send(c, &pong).await;
            }
            _ => return Ok(()),
        }
    }
}

async fn login(sh: &Arc<Shared>, c: &mut TcpStream, hs: &[u8], ip: IpAddr) -> io::Result<()> {
    let p = within(read_packet(c, MAX_CLIENT_PACKET)).await?;
    let mut b = &p[..];
    if get_varint(&mut b)? != 0 {
        return Err(bad("expected login start"));
    }
    let name = String::from_utf8_lossy(get_bytes(&mut b)?).into_owned();
    let msg = match sh.on_join(&name, ip) {
        Join::Kick(msg) => msg,
        Join::Resume if sh.resume().await => return forward(sh, c, &[hs, &p[..]]).await,
        Join::Resume => "§6The server is waking up!§r\n\nTry again in a few seconds.".into(),
    };
    send_kick(c, &msg).await
}

/// Login-phase disconnect with a chat message.
async fn send_kick(c: &mut TcpStream, msg: &str) -> io::Result<()> {
    send(c, &string_packet(&json!({"text": msg}).to_string())).await?;
    c.shutdown().await.ok();
    Ok(())
}

// ---- monitor: polls the backend, tracks the phase, stops it when idle ---------------------------

async fn monitor(sh: Arc<Shared>) {
    let cfg = &sh.cfg;
    loop {
        if sh.phase() == Phase::Frozen {
            // A stopped process still accepts TCP connections but never answers: don't ping it.
            sh.check_frozen();
            sleep(Duration::from_secs(cfg.poll_secs.max(1))).await;
            continue;
        }
        let reply = ping_backend(&cfg.backend, cfg.protocol).await;
        let now = Instant::now();
        let mut save: Option<String> = None;
        let mut start_now = false;
        let mut stop_now = false;
        let mut freeze_now: Option<u64> = None;
        {
            let mut st = sh.st.lock().unwrap();
            match (reply, st.phase) {
                (Ok(js), phase) => {
                    let online = players_online(&js);
                    // A server that still answers right after we told it to stop is just shutting down.
                    let shutting_down = phase == Phase::Stopping && st.since.elapsed() < Duration::from_secs(300);
                    if phase != Phase::Up && !shutting_down {
                        sh.became_up(&mut st, online);
                    }
                    if st.phase == Phase::Up {
                        // Unknown player count counts as active: never stop on a guess.
                        if online != Some(0) || sh.conns.load(Ordering::Relaxed) > 0 {
                            st.last_active = now;
                        }
                        let idle = now.duration_since(st.last_active);
                        if idle >= Duration::from_secs(cfg.idle_secs)
                            && st.since.elapsed() >= Duration::from_secs(cfg.min_up_secs)
                        {
                            slog!(
                                sh,
                                "info",
                                "idle",
                                idle_secs = idle.as_secs(),
                                mode = format!("{:?}", cfg.sleep_mode)
                            );
                            if cfg.sleep_mode == SleepMode::Freeze {
                                freeze_now = Some(st.since.elapsed().as_secs());
                            } else {
                                set_phase(&mut st, Phase::Stopping);
                                stop_now = true;
                            }
                        }
                    }
                    let fresh = st.cache_saved.is_some_and(|t| t.elapsed() <= Duration::from_secs(600));
                    if !fresh && online.is_some() {
                        st.cache_saved = Some(now);
                        save = Some(js.clone());
                    }
                    st.cache = Some(js);
                }
                (Err(_), Phase::Up) => {
                    slog!(sh, "warn", "down", note = "backend stopped answering");
                    set_phase(&mut st, Phase::Sleeping);
                }
                (Err(_), Phase::Stopping) => {
                    slog!(sh, "info", "asleep");
                    set_phase(&mut st, Phase::Sleeping);
                    if st.wake_pending {
                        st.wake_pending = false;
                        set_phase(&mut st, Phase::Starting);
                        start_now = true;
                    }
                }
                (Err(_), Phase::Starting) => {
                    if st.since.elapsed() > Duration::from_secs(cfg.wake_timeout_secs) {
                        slog!(sh, "error", "wake_timeout", secs = cfg.wake_timeout_secs);
                        set_phase(&mut st, Phase::Sleeping);
                    }
                }
                (Err(_), Phase::Sleeping | Phase::Frozen) => {}
            }
        }
        if let Some(up_secs) = freeze_now {
            match freeze::freeze(cfg, up_secs).await {
                Ok(f) if sh.conns.load(Ordering::Relaxed) > 0 => {
                    // Someone connected while we were freezing: undo it.
                    freeze::resume(&f).await;
                    slog!(sh, "warn", "freeze_aborted", note = "a connection arrived");
                }
                Ok(f) => {
                    slog!(sh, "info", "frozen", pid = f.pid);
                    let mut st = sh.st.lock().unwrap();
                    set_phase(&mut st, Phase::Frozen);
                    st.frozen = Some(f);
                }
                Err(why) => {
                    slog!(
                        sh,
                        "error",
                        "freeze_refused",
                        reason = why,
                        note = "falling back to stop mode for this sleep"
                    );
                    set_phase(&mut sh.st.lock().unwrap(), Phase::Stopping);
                    stop_now = true;
                }
            }
        }
        if stop_now {
            sh.run("stop", cfg.stop_cmd.clone());
        }
        if start_now {
            slog!(sh, "info", "wake", player = "(queued)");
            sh.run("start", cfg.start_cmd.clone());
        }
        if let (Some(js), Some(path)) = (save, &cfg.cache_file) {
            let tmp = path.with_extension("tmp");
            if tokio::fs::write(&tmp, js).await.is_ok() {
                tokio::fs::rename(&tmp, path).await.ok();
            }
        }
        // Poll fast while waiting for a boot so the joiner can get in as soon as it is up.
        let secs = if sh.phase() == Phase::Starting { 1 } else { cfg.poll_secs.max(1) };
        sleep(Duration::from_secs(secs)).await;
    }
}

// ---- entry point --------------------------------------------------------------------------------

/// `--check`: for every server, ping the backend once and print what dormant saw and what it would
/// serve while asleep.
async fn check(l: cli::Loaded) -> ExitCode {
    println!(
        "listen {}, {} server(s), default: {}",
        l.listen,
        l.servers.len(),
        l.default.as_deref().unwrap_or("(none)")
    );
    let mut all_ok = true;
    for cfg in l.servers {
        println!("== {} (hosts: {})", cfg.name, if cfg.hosts.is_empty() { "any".into() } else { cfg.hosts.join(", ") });
        all_ok &= check_one(cfg).await;
    }
    if all_ok {
        ExitCode::SUCCESS
    } else {
        ExitCode::from(1)
    }
}

async fn check_one(cfg: Config) -> bool {
    let t0 = Instant::now();
    let reply = ping_backend(&cfg.backend, cfg.protocol).await;
    let sh = Shared::new(cfg);
    let ok = match &reply {
        Ok(js) => {
            println!(
                "backend {}: answered in {} ms, {} bytes of status JSON",
                sh.cfg.backend,
                t0.elapsed().as_millis(),
                js.len()
            );
            match serde_json::from_str::<Value>(js) {
                Ok(v) => {
                    let keys: Vec<&str> =
                        v.as_object().map(|o| o.keys().map(String::as_str).collect()).unwrap_or_default();
                    println!("  top-level keys: {}", keys.join(", "));
                    println!("  version: {}", v.get("version").map_or("-".into(), |x| x.to_string()));
                    println!(
                        "  players online: {}",
                        v.pointer("/players/online").map_or("-".into(), |x| x.to_string())
                    );
                    println!("  description: {}", v.get("description").map_or("-".into(), |x| x.to_string()));
                }
                Err(e) => println!(
                    "  note: reply is not valid JSON ({e}); it is kept as is and the sleeping reply is synthesized"
                ),
            }
            sh.st.lock().unwrap().cache = Some(js.clone());
            true
        }
        Err(e) => {
            println!("backend {}: no status reply ({e}) - is the server running?", sh.cfg.backend);
            println!(
                "  cache file: {}",
                if sh.st.lock().unwrap().cache.is_some() { "loaded, used for the sleeping reply" } else { "none" }
            );
            false
        }
    };
    let s = sh.sleeping_status();
    println!(
        "sleeping reply: {} bytes, description {}",
        s.len(),
        serde_json::from_str::<Value>(&s).ok().and_then(|v| v.get("description").cloned()).unwrap_or_default()
    );
    {
        let st = sh.st.lock().unwrap();
        println!(
            "wake time: {} s ({}; recent boots: {:?})",
            sh.wake_secs(&st),
            if st.boots.is_empty() { "wake_secs_default, none measured yet" } else { "median of measured" },
            st.boots
        );
    }
    for line in freeze::describe(&sh.cfg) {
        println!("{line}");
    }
    ok
}

const USAGE: &str = "usage: dormant [--check] <config.toml>   run the proxy, or check the config and backends once
       dormant list [config.toml]            show every server and its state
       dormant new <name> ...                add a server to a config file (see `dormant new --help`)
       dormant --version";

#[tokio::main]
async fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        Some("new") => return cli::cmd_new(&args[1..]),
        Some("list") if !args.iter().any(|a| a.starts_with('-')) => return cli::cmd_list(&args[1..]).await,
        _ => {}
    }
    if args.iter().any(|a| a == "--version" || a == "-V") {
        println!("dormant {}", env!("CARGO_PKG_VERSION"));
        return ExitCode::SUCCESS;
    }
    if args.iter().any(|a| a == "--help" || a == "-h") {
        println!("{USAGE}");
        return ExitCode::SUCCESS;
    }
    let Some(path) = args.iter().find(|a| !a.starts_with('-')) else {
        eprintln!("{USAGE}");
        return ExitCode::from(2);
    };
    let loaded = match cli::load(path) {
        Ok(l) => l,
        Err(e) => {
            eprintln!("config error: {e}");
            return ExitCode::from(2);
        }
    };
    if args.iter().any(|a| a == "--check") {
        return check(loaded).await;
    }

    let listener = match TcpListener::bind(&loaded.listen).await {
        Ok(l) => l,
        Err(e) => {
            eprintln!("cannot listen on {}: {e}", loaded.listen);
            return ExitCode::from(1);
        }
    };
    let servers: Vec<Arc<Shared>> = loaded.servers.into_iter().map(Shared::new).collect();
    for sh in &servers {
        slog!(
            sh,
            "info",
            "serving",
            hosts = sh.cfg.hosts.join(","),
            backend = sh.cfg.backend,
            mode = format!("{:?}", sh.cfg.sleep_mode)
        );
        tokio::spawn(monitor(sh.clone()));
    }
    let default = loaded
        .default
        .as_ref()
        .and_then(|d| servers.iter().position(|s| &s.cfg.name == d))
        .or_else(|| (servers.len() == 1 && servers[0].cfg.hosts.is_empty()).then_some(0));
    log!(
        "info",
        "listening",
        listen = loaded.listen,
        servers = servers.len(),
        default = default.map_or("-".into(), |i| servers[i].cfg.name.clone())
    );
    let rt = Arc::new(Router { servers, default, unknown_motd: loaded.unknown_motd });
    let slots = Arc::new(Semaphore::new(MAX_CONNECTIONS));
    let mut term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()).expect("signal handler");
    loop {
        tokio::select! {
            r = listener.accept() => {
                let Ok((sock, _)) = r else { continue };
                let Ok(permit) = slots.clone().try_acquire_owned() else { continue };
                let rt = rt.clone();
                tokio::spawn(async move {
                    let _permit = permit;
                    if let Err(e) = handle(rt, sock).await {
                        if !matches!(e.kind(), io::ErrorKind::UnexpectedEof | io::ErrorKind::ConnectionReset | io::ErrorKind::TimedOut | io::ErrorKind::BrokenPipe) {
                            log!("debug", "client_error", error = e);
                        }
                    }
                });
            }
            _ = term.recv() => break,
            _ = tokio::signal::ctrl_c() => break,
        }
    }
    log!("info", "shutdown", note = "backend left as it is");
    ExitCode::SUCCESS
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_cfg(extra: &str) -> Config {
        toml::from_str(&format!(
            "listen=\"x\"\nbackend=\"y\"\nstart_cmd=\"true\"\nstop_cmd=\"true\"\nmotd_sleeping=\"zzz\"\n{extra}"
        ))
        .unwrap()
    }

    #[test]
    fn varint_round_trip() {
        for v in [0, 1, 127, 128, 340, 25565, 2_097_151, i32::MAX, -1] {
            let mut out = Vec::new();
            put_varint(&mut out, v);
            assert_eq!(get_varint(&mut &out[..]).unwrap(), v);
        }
    }

    #[test]
    fn sleeping_status_keeps_forge_data_and_replaces_description() {
        let sh = Shared::new(test_cfg(""));
        // Forge 1.12.2 style: description is an object, modinfo carries the mod list.
        sh.st.lock().unwrap().cache = Some(r#"{"description":{"text":"real"},"players":{"max":20,"online":3,"sample":[{"name":"a","id":"b"}]},"version":{"name":"1.12.2","protocol":340},"favicon":"data:x","modinfo":{"type":"FML","modList":[{"modid":"m","version":"1"}]}}"#.into());
        let v: Value = serde_json::from_str(&sh.sleeping_status()).unwrap();
        assert_eq!(v["description"]["text"], "zzz");
        assert_eq!(v["players"]["online"], 0);
        assert_eq!(v["players"]["max"], 20);
        assert!(v["players"].get("sample").is_none());
        assert_eq!(v["modinfo"]["modList"][0]["modid"], "m");
        assert_eq!(v["favicon"], "data:x");
    }

    #[test]
    fn sleeping_status_survives_garbage_cache() {
        let sh = Shared::new(test_cfg("max_players=5"));
        sh.st.lock().unwrap().cache = Some("not json {".into());
        let v: Value = serde_json::from_str(&sh.sleeping_status()).unwrap();
        assert_eq!(v["version"]["protocol"], 340);
        assert_eq!(v["players"]["max"], 5);
    }

    fn kick(j: Join) -> String {
        match j {
            Join::Kick(m) => m,
            Join::Resume => "RESUME".into(),
        }
    }
    fn ip(n: u8) -> IpAddr {
        IpAddr::from([203, 0, 113, n])
    }

    #[tokio::test]
    async fn whitelist_and_phases() {
        let sh = Shared::new(test_cfg("wake_names=[\"Steve\"]"));
        assert!(kick(sh.on_join("Mallory", ip(1))).contains("not allowed"));
        assert_eq!(sh.phase(), Phase::Sleeping);
        assert!(kick(sh.on_join("steve", ip(1))).contains("starting"));
        assert_eq!(sh.phase(), Phase::Starting);
    }

    #[tokio::test]
    async fn wake_limits_apply_only_to_logins_that_would_wake() {
        let sh = Shared::new(test_cfg("wake_limit_per_ip=1\nwake_limit_global=2\nkick_rate_limited=\"slow down\""));
        let asleep = |sh: &Arc<Shared>| set_phase(&mut sh.st.lock().unwrap(), Phase::Sleeping);
        assert!(kick(sh.on_join("a", ip(1))).contains("starting"));
        // while it is starting, more logins from the same address are free and get the countdown
        assert!(kick(sh.on_join("a", ip(1))).contains("starting"));
        asleep(&sh);
        assert_eq!(kick(sh.on_join("a", ip(1))), "slow down"); // second wake from the same address
        assert_eq!(sh.phase(), Phase::Sleeping); // and nothing was started
        assert!(kick(sh.on_join("b", ip(2))).contains("starting"));
        asleep(&sh);
        assert_eq!(kick(sh.on_join("c", ip(3))), "slow down"); // third wake overall
        assert_eq!(sh.phase(), Phase::Sleeping);
        // a refused login must not queue a wake while the server is stopping either
        set_phase(&mut sh.st.lock().unwrap(), Phase::Stopping);
        assert_eq!(kick(sh.on_join("c", ip(3))), "slow down");
        assert!(!sh.st.lock().unwrap().wake_pending);
    }

    #[test]
    fn wake_limit_defaults() {
        let c = test_cfg("");
        assert_eq!((c.wake_limit_per_ip, c.wake_limit_global, c.wake_limit_window_secs), (3, 6, 600));
    }

    #[test]
    fn hostname_routing() {
        assert_eq!(host_of(b"MC.Example.Com.\0FML\0"), "mc.example.com");
        assert_eq!(host_of(b"modded.example.com\0FORGE"), "modded.example.com");
        assert_eq!(host_of(b""), "");
        let mk = |name: &str, hosts: &str| {
            let mut c = test_cfg(&format!("hosts=[{hosts}]"));
            c.name = name.into();
            Shared::new(c)
        };
        let rt = Router {
            servers: vec![mk("alpha", "\"alpha.x\",\"mc.x\""), mk("beta", "\"beta.x\"")],
            default: Some(0),
            unknown_motd: "u".into(),
        };
        assert_eq!(rt.route("beta.x").unwrap().cfg.name, "beta");
        assert_eq!(rt.route("mc.x").unwrap().cfg.name, "alpha");
        assert_eq!(rt.route("nope.x").unwrap().cfg.name, "alpha"); // default server
        let rt = Router { default: None, ..rt };
        assert!(rt.route("nope.x").is_none());
    }

    #[test]
    fn median_of_last_five() {
        assert_eq!(median(&[]), None);
        assert_eq!(median(&[70]), Some(70));
        assert_eq!(median(&[90, 60, 75]), Some(75));
        assert_eq!(median(&[60, 80]), Some(70));
        assert_eq!(median(&[100, 71, 70, 72, 300]), Some(72)); // one outlier does not move it
    }

    #[test]
    fn wake_secs_placeholder_in_motd_and_kick() {
        let mut cfg = test_cfg("wake_secs_default=90\nkick_starting=\"try in {secs} of {wake_secs}\"");
        cfg.motd_sleeping = "asleep (~{wake_secs} s)".into();
        let sh = Shared::new(cfg);
        let motd = |sh: &Shared| {
            serde_json::from_str::<Value>(&sh.sleeping_status()).unwrap()["description"]["text"]
                .as_str()
                .unwrap()
                .to_string()
        };
        assert_eq!(motd(&sh), "asleep (~90 s)"); // default until a boot has been measured
        sh.st.lock().unwrap().boots = vec![70, 72, 71];
        assert_eq!(motd(&sh), "asleep (~71 s)");
        assert_eq!(sh.kick_text(Phase::Starting, 71, 30), "try in 30 of 71");
        // old config key still accepted
        assert_eq!(test_cfg("boot_estimate_secs=45").wake_secs_default, 45);
    }

    #[test]
    fn became_up_keeps_the_last_five() {
        let sh = Shared::new(test_cfg(""));
        let mut st = sh.st.lock().unwrap();
        for _ in 0..7 {
            set_phase(&mut st, Phase::Starting);
            sh.became_up(&mut st, Some(0));
        }
        assert_eq!(st.boots.len(), 5);
        assert_eq!(st.phase, Phase::Up);
    }
}
