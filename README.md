# dormant

A tiny sleep proxy for Minecraft Java servers. It owns the public port, answers server-list pings
while the real server is off, starts the server when a player tries to join, and stops (or
freezes) it again when it has been empty for a while. Works with vanilla, Paper, Fabric, NeoForge
and Forge, including Forge 1.12.2.

What sets it apart:

- **Modded servers just work.** The sleeping server-list entry is the server's own last status
  reply, kept as an opaque string with only the MOTD and the player count replaced. Mod list,
  icon and version stay exactly as the real server sent them, and the handshake is replayed to the
  server byte for byte.
- **Several servers on one port**, chosen by the hostname the player typed.
- **Freeze mode**: pause the server process instead of stopping it, and let the joining player
  straight in when it resumes.
- **Honest wake times**: the MOTD and kick messages show the measured time your server takes to
  wake, not a guess.

It is one static binary with no runtime dependencies, written in Rust (tokio, serde, toml; no
`unsafe`). It runs on Linux.

## Quick start

You need a Minecraft server that you can start and stop with a command, and
[Rust](https://rustup.rs) to build dormant (or a binary from the releases page, if there is one for
your platform).

1. **Build and install.**

       git clone https://github.com/BrennanWirt/dormant
       cd dormant
       cargo build --release
       sudo install -m 755 target/release/dormant /usr/local/bin/dormant

2. **Move the real server off the public port.** In its `server.properties` set
   `server-port=25566` and `server-ip=127.0.0.1`, then restart it and leave it running for step 4.

3. **Write `/etc/dormant/dormant.toml`.** These six lines are a complete config; replace the two
   commands with whatever starts and stops your server:

       listen  = "0.0.0.0:25565"
       backend = "127.0.0.1:25566"
       start_cmd = "systemctl start --no-block minecraft"
       stop_cmd  = "systemctl stop --no-block minecraft"
       motd_sleeping = "§6My server\n§7Asleep - join to wake it (~{wake_secs} s)"
       cache_file = "/var/lib/dormant/status.json"

   Create the directory for the cache file: `sudo mkdir -p /var/lib/dormant`.

4. **Check it.** With the server running:

       dormant --check /etc/dormant/dormant.toml

   This pings the server once and prints what it answered and what dormant would show while it is
   asleep.

5. **Run it.**

       sudo dormant /etc/dormant/dormant.toml

   Add the server in your Minecraft client. Once it has been empty for 15 minutes, dormant runs
   `stop_cmd` and the server list shows your sleeping MOTD. Join, and you are told "The server is
   starting! Try again in about N seconds"; join again after that and you are in.

To keep it running, install it as a service: see [examples/systemd](examples/systemd). There are
also examples for a [Docker container](examples/docker) and for
[Crafty Controller](examples/crafty), a fully commented [config](examples/dormant.toml) and a
[multi-server config](examples/multi-server.toml).

## Commands

    dormant <config.toml>            run the proxy
    dormant --check <config.toml>    ping every backend once; print what it answered, the sleeping
                                     reply dormant would serve, the wake time and (freeze mode) which
                                     process would be paused and whether freezing is allowed
    dormant list [config.toml]       table of all servers: hosts, mode, state, wake time, backend
    dormant new <name> ...           append a [[server]] block to a config (see below)
    dormant --version

`list` and `new` use `/etc/dormant/dormant.toml` when no config is named. Logs go to stderr as
`key=value` lines (`level=info event=wake server=main player=Steve`).

## How it works

**While the server is up**, every connection is forwarded untouched. dormant reads only the first
packet (the handshake) to pick the server, then replays it unchanged, including the `\0FML\0`
suffix that Forge clients add to the address, and copies bytes in both directions.

**Idle detection.** Every `poll_secs` dormant asks the real server for its status, exactly like a
client refreshing its server list, and reads `players.online` from the answer. The server counts as
active while that number is above zero, while the number is missing or unreadable, or while any
proxied connection is open. After `idle_secs` without activity, and no sooner than `min_up_secs`
after the server came up, dormant runs `stop_cmd` (or freezes the process).

**While the server is off**, dormant answers status requests itself. The reply is the last real
status reply with `description` replaced by your MOTD, `players.online` set to 0 and the player
sample removed. Nothing else in it is interpreted, so whatever your server puts there (`modinfo`,
`forgeData`, `favicon`, ...) is passed on. The reply is saved to `cache_file` about every ten
minutes so that it survives a restart of dormant. Until a real reply has been seen, a minimal one is
made from `version_name` and `protocol`.

**Waking.** Only a login wakes the server; a status ping never does. When a player whose name is
allowed (`wake_names`; default: anyone) tries to join, dormant runs `start_cmd` and disconnects the
player with `kick_starting`. It then polls the server once a second. The server counts as up when
its status reply contains `players.online`. (Forge 1.12.2 answers status requests with a stub long
before it accepts logins; the stub has no player count, so dormant keeps waiting.) If the server
does not come up within `wake_timeout_secs`, or `start_cmd` fails, dormant goes back to sleeping.
A join attempt while the server is still shutting down is remembered, and the server is started
again as soon as it is down.

**Measured wake time.** Each wake is timed from `start_cmd` to the first complete status reply. The
median of the last five is stored in `state_file` and replaces `{wake_secs}` in MOTDs and kick
texts; `{secs}` in a kick text is the time left of the wake in progress. Until one wake has been
measured, `wake_secs_default` is used.

**dormant does not own the server process.** It only runs your two commands and watches the port.
You can restart dormant at any time without touching the server (connected players are dropped),
and you can start or stop the server by hand: dormant notices either way.

## Configuration reference

The config is one TOML file. Unknown keys are an error. Every key below except the first five is
optional.

| Key | Default | Meaning |
| --- | --- | --- |
| `listen` | required | Address and port players connect to, e.g. `"0.0.0.0:25565"`. |
| `backend` | required | Address and port of the real server, e.g. `"127.0.0.1:25566"`. |
| `start_cmd` | required | Shell command that starts the server. Run with `sh -c`; must return quickly (it is killed after 180 s). |
| `stop_cmd` | required | Shell command that stops the server. Same rules. |
| `motd_sleeping` | required | MOTD while the server is off. May contain `{wake_secs}`. |
| `motd_starting` | `motd_sleeping` | MOTD while the server is starting. |
| `motd_stopping` | `motd_sleeping` | MOTD while the server is shutting down. |
| `kick_starting` | "The server is starting! Try again in about {secs} seconds." | Shown to a player whose join started the server, or who joins while it starts. May contain `{secs}` and `{wake_secs}`. |
| `kick_stopping` | "The server is going to sleep. Try again in about a minute and it will wake up." | Shown to a player who joins while the server shuts down. |
| `kick_denied` | "The server is asleep and you are not allowed to wake it." | Shown to a player who is not in `wake_names`. |
| `kick_rate_limited` | "Too many wake-ups in a short time. Try again in a few minutes." | Shown when a wake limit refuses the join. |
| `idle_secs` | `900` | Sleep after this many seconds without players. |
| `min_up_secs` | `600` | Never sleep sooner than this after the server came up. |
| `wake_secs_default` | `90` | Value of `{wake_secs}` until a wake has been measured. (Old name `boot_estimate_secs` is still accepted.) |
| `wake_timeout_secs` | `600` | Give up on a start that has not answered after this long. |
| `poll_secs` | `5` | Seconds between status pings to the real server (1 second while it is starting). |
| `wake_names` | `[]` | If not empty, only these player names (case-insensitive) may wake the server. See [Security](#security). |
| `wake_limit_per_ip` | `3` | Wakes allowed per source address within the window. `0` = unlimited. |
| `wake_limit_global` | `6` | Wakes allowed in total for this server within the window. `0` = unlimited. |
| `wake_limit_window_secs` | `600` | Length of the rate-limit window. |
| `cache_file` | none | File that keeps the last real status reply across restarts of dormant. The directory must exist and be writable. |
| `state_file` | `cache_file` with the extension `.state.json` | File that keeps the measured wake times. Without `cache_file` and `state_file` they are forgotten on restart. |
| `max_players` | from the last real reply, else `20` | Player limit shown while the server is off. |
| `version_name` | `"1.12.2"` | Version shown while off, only until a real reply has been cached. |
| `protocol` | `340` | Protocol number sent in dormant's own status pings and shown while off until a real reply has been cached. Servers answer a status ping whatever the number is; set it to your version's number so that clients do not show the sleeping server as outdated before the first real reply is cached. |
| `sleep_mode` | `"stop"` | `"stop"` runs `stop_cmd`; `"freeze"` pauses the process. See [Freeze mode](#freeze-mode). |
| `properties_file` | none | Freeze mode: path of the server's `server.properties`. Required for freezing. |
| `pidfile` | none | Freeze mode: file containing the server's process id. |
| `pid_pattern` | none | Freeze mode: text that occurs in the command line of exactly one process, the server. |
| `name` | `"main"`, or `"server1"`, `"server2"`, ... | Name used in logs and by `dormant list`. |
| `hosts` | `[]` | Hostnames that select this server. See below. |

In a multi-server file there are three more keys at the top level, next to `listen`:

| Key | Default | Meaning |
| --- | --- | --- |
| `default` | none | Name of the server that receives hostnames no server claims. |
| `unknown_motd` | "Unknown server / Check the address you typed" | MOTD for unclaimed hostnames when there is no default server. |
| `[[server]]` | | One block per server, with the keys of the first table except `listen`. |

MOTDs and kick texts are plain text with the usual `§` colour codes; `\n` starts a new line.

## Several servers on one port

A Minecraft client sends the address the player typed in its first packet. dormant uses it to
choose a server, so several servers can share one IP address and port as long as each has its own
DNS name.

    listen = "0.0.0.0:25565"
    default = "survival"

    [[server]]
    name = "survival"
    hosts = ["mc.example.com", "survival.example.com"]
    backend = "127.0.0.1:25566"
    start_cmd = "systemctl start --no-block minecraft@survival"
    stop_cmd  = "systemctl stop --no-block minecraft@survival"
    motd_sleeping = "§6Survival\n§7Asleep - join to wake it (~{wake_secs} s)"
    cache_file = "/var/lib/dormant/survival-status.json"

    [[server]]
    name = "modded"
    hosts = ["modded.example.com"]
    backend = "127.0.0.1:25567"
    ...

- Matching ignores case, a trailing dot and anything after a NUL byte (Forge's `\0FML\0`).
- Each server sleeps, wakes and is rate limited on its own, and shows its own MOTD.
- A hostname that no server claims goes to the `default` server. Without `default` it gets the
  `unknown_motd` and a login is refused with "Unknown server"; nothing is woken.
- Names, hostnames and cache/state files must be unique; a config that breaks this is refused.
- A file without `[[server]]` is a single-server config: it answers every hostname.
- dormant does not coordinate memory: two large servers woken at the same time both run.

`dormant new` appends a block and validates the result before writing it (the old file is kept as
`<config>.bak`), or creates the file if it does not exist:

    dormant new modded --backend-port 25567 --hosts modded.example.com \
        --start-cmd "systemctl start --no-block minecraft@modded" \
        --stop-cmd  "systemctl stop --no-block minecraft@modded"

Options: `--mode stop|freeze`, `--properties-file <path>` (needed for freeze), `--config <path>`,
and `--crafty-id <id>` as a shortcut for the two commands when the server is managed by Crafty
Controller ([examples/crafty](examples/crafty)). There is no live reload: restart dormant to pick
up a changed config.

## Freeze mode

With `sleep_mode = "freeze"`, an idle server is paused with `SIGSTOP` instead of being stopped. It
keeps its memory and its loaded world but uses no CPU. A login resumes it with `SIGCONT`; dormant
waits up to 3 seconds for it to answer and then forwards the joining player directly, without a
kick. Status pings are answered from the cache and never resume the server. Linux only (it reads
`/proc`).

Pausing a JVM from outside is only safe under some conditions, so dormant checks them every time
and otherwise falls back to `stop_cmd` for that sleep, with an error in the log
(`event=freeze_refused`):

- **`max-tick-time=-1` must be set in `server.properties`**, and `properties_file` must point to
  that file so dormant can verify it. A resumed server believes one tick took as long as the
  freeze lasted; with the default watchdog setting it would kill itself. Forge has a watchdog of
  its own in its config; dormant cannot check that one, so check it yourself before using freeze
  mode with Forge.
- `min_up_secs` must have passed since the server came up.
- After `SIGSTOP` the process must really be in the stopped state; if not, it is resumed and the
  freeze is refused.

Which process is paused: the one named in `pidfile`; else the single process whose command line
contains `pid_pattern`; else the process that owns the listening socket of the backend port
(dormant must run as root to find it). If the server leads its own process group, the whole group
is signalled; otherwise the server and its child processes. A group that the server shares with its
parent (a panel, a wrapper script) is never signalled, so the parent keeps running.

Other things to know:

- Run `dormant --check` first: it prints the process and signal targets it would use and whether
  the `max-tick-time` check passes.
- If dormant is restarted while the server is frozen, it recognises the frozen process and treats
  it as asleep. If something else resumes the process, dormant notices and treats it as starting.
- If dormant is stopped for good while the server is frozen, the server stays frozen. Resume it
  with `kill -CONT <pid>`.
- A second player who joins during the up-to-3-second resume is asked to try again in a few
  seconds.
- A frozen server holds its memory. Freeze mode trades RAM for an instant wake; it does not free
  the machine for other work the way stopping does.
- A freeze never happens while a player is connected: an open connection counts as activity.

## Compared with lazymc and mc-router

Both are mature, widely used projects with far more features than dormant. The statements about
them below come from their READMEs and example configs at the time of writing; check the projects
themselves for the current state.

[**lazymc**](https://github.com/timvisee/lazymc) (Rust, GPL-3.0) is the best-known sleep proxy.

- lazymc offers four ways to treat a joining player (kick, hold the connection until the server
  is up, forward to another address, or an experimental lobby world). dormant only kicks, or in
  freeze mode forwards.
- lazymc supports the PROXY protocol, blocks IPs from the server's ban list, can stop the server
  through RCON, restarts it after a crash, manages `server.properties` for you and supports
  Windows and macOS. dormant has none of that.
- lazymc also has an option to freeze the server process instead of stopping it.
- lazymc's configuration describes one public address and one server. dormant routes several
  servers on one port by hostname.
- dormant treats the server's status reply as opaque data and replays the handshake unchanged,
  with no Forge-specific setting; that is the reason it exists. lazymc has a `forge` option.
- dormant never owns the server process: it runs a start and a stop command and can be restarted
  independently of the server.
- dormant measures the real wake time and shows it to players, and rate limits wakes.

[**mc-router**](https://github.com/itzg/mc-router) is a hostname router for many backend servers.

- mc-router routes by hostname with mappings from the command line, a file, a REST API, or Docker
  and Kubernetes discovery. It supports the PROXY protocol, connection rate limiting, client
  allow/deny lists, webhooks and metrics. dormant has static routing from one config file.
- mc-router can scale Kubernetes StatefulSets to zero and back, start and stop Docker containers,
  or call a webhook, and can show an "asleep" MOTD meanwhile. dormant runs arbitrary shell
  commands instead, so it fits servers that are plain processes, systemd units or panel-managed.
- dormant's sleeping reply is the server's own cached status (mod list, icon, version), and it
  offers freeze mode and measured wake times.

If you run containers under Kubernetes or need real client addresses, use mc-router. If you run one
server and want players held in a queue or lobby while it starts, use lazymc. dormant is for a
handful of (often modded) servers on one machine that should look real while asleep.

## Limitations

- Minecraft **Java Edition** only, and only the modern protocol (1.7 and later). The legacy
  server-list ping of older clients is not answered. Bedrock is not supported.
- **No PROXY protocol**, in either direction. The real server sees every player coming from
  dormant's address, so IP bans and per-IP limits inside the server do not work.
- **No live reload.** A config change needs a restart, which drops connected players.
- A joining player is kicked once and has to join again (except in freeze mode).
- Linux only. It will not build on Windows. Other Unix systems are untested, and freeze mode needs
  `/proc`.
- At most 512 concurrent connections.
- Players who connect straight to the backend port are not seen as connections, only through the
  player count.

## Security

dormant listens on a public port, and a login attempt makes it run a command. What limits that:

- **`start_cmd` and `stop_cmd` come from the config file only.** They are passed to `sh -c` exactly
  as written; nothing a client sends (player name, hostname) is ever put into a command or a file
  name. Keep the config writable by root only, because whoever can edit it can run commands as
  the user dormant runs as.
- **Wake rate limits.** A wake is a login that starts the server, resumes it from a freeze, or
  queues a start while it is shutting down. Per server, at most `wake_limit_per_ip` wakes are
  accepted from one source address (an IPv4 address or an IPv6 /64) and `wake_limit_global` from
  all addresses together, within `wake_limit_window_secs`; further attempts are refused with
  `kick_rate_limited` and logged as `event=wake_rate_limited`. The defaults (3 and 6 per 10
  minutes) are far above normal use, since a server that was woken stays up for at least
  `min_up_secs`. Logins while the server is already starting or running are not counted. If
  dormant sits behind a tunnel or another proxy, every player has the same source address and the
  per-address limit applies to all of them together.
- **`wake_names` is an allowlist, not authentication.** dormant sees the player name a client
  claims before any login with Mojang's servers happens, so anyone who knows an allowed name can
  wake the server. They still cannot join: the real server authenticates and applies its own
  whitelist. Use `wake_names` to stop scanners and strangers from waking the server, together with
  the rate limits.
- The rate limits bound how often the server is started, not how long it runs: one allowed wake
  keeps it up for `min_up_secs` plus `idle_secs`.
- Client packets are limited to 4 KiB before forwarding starts, every step of the handshake has a
  15 second timeout, and there is a cap of 512 connections. There is no per-address connection
  limit; use your firewall for that.
- In freeze mode dormant sends `SIGSTOP`/`SIGCONT` to the process it identified as described above.
  Prefer `pidfile` or `pid_pattern` if anything else could ever listen on the backend port.

## Testing

`cargo test` runs the unit tests. `harness/run.sh` runs dormant against fake backends
(`harness/fakemc.py`, Python 3) on localhost and checks routing, waking, idle stop, the rate limit
and freeze mode end to end; see the comments at its top. `harness/mcclient.py` is a small
command-line client for pinging or attempting a login by hand.

## License

Licensed under either of [Apache License, Version 2.0](LICENSE-APACHE) or [MIT license](LICENSE-MIT)
at your option. Unless you explicitly state otherwise, any contribution intentionally submitted for
inclusion in this project by you, as defined in the Apache-2.0 license, shall be dual licensed as
above, without any additional terms or conditions.
