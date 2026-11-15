# Beyond Minecraft Java: a design note

This is a sketch, not a plan of record. Nothing here is implemented. It describes how dormant
could wake and sleep other kinds of servers without losing what makes it small, and what that
would cost.

## What is already generic

Most of dormant does not know it is talking to Minecraft:

- the phase machine (`Sleeping`, `Starting`, `Up`, `Stopping`, `Frozen`) and the `monitor` loop,
- running `start_cmd` / `stop_cmd`, the wake timeout, the queued wake while stopping,
- freeze mode (`freeze.rs`), which only deals with processes,
- wake rate limits (`limit.rs`) and the `wake_names` idea (an allowlist on some client identity),
- measured wake times and the `{wake_secs}` / `{secs}` placeholders,
- config loading, `list`, `new`, logging.

What is Minecraft-specific sits in four places in `main.rs`:

1. `handle` / `login` / `status_loop`: parsing the handshake, telling a status ping from a login,
   and answering each while the server is off.
2. `sleeping_status` and `kick_text`: what a client is shown while the server is off.
3. `ping_backend` / `players_online`: the idle probe (is it up, is anybody on it).
4. `host_of` and `Router::route`: routing by the hostname inside the handshake.

Generalising means putting a seam around those four and nothing else.

## The seam: one trait per protocol family

    /// What a protocol family has to provide. One implementation per family.
    trait Protocol: Send + Sync + 'static {
        /// Look at the first bytes of a new connection (or the first datagram).
        /// Decide what it is, and which server it is for if the protocol carries a name.
        fn classify(&self, first: &[u8]) -> Classified;

        /// Bytes to answer a status / browser query with while the server is not up.
        /// `cached` is the last real answer, if the family has such a thing.
        fn sleeping_reply(&self, query: &[u8], cached: Option<&[u8]>, ctx: &Texts) -> Option<Vec<u8>>;

        /// Bytes to send to a client that tried to join and has to come back later
        /// (None: just close, or hold the connection, depending on the mode).
        fn refuse_join(&self, join: &[u8], message: &str) -> Option<Vec<u8>>;

        /// Ask the real server whether it is up and, if the protocol can tell, how many
        /// players are on it. `Unknown` counts as active, as today.
        async fn probe(&self, backend: &str) -> io::Result<Probe>;
    }

    enum Classified {
        NeedMore,                                   // not enough bytes yet
        Status { host: Option<String> },            // answer it ourselves while asleep
        Join { host: Option<String>, who: Option<String> },   // counts as a wake request
        Other,                                      // forward if up, drop if not
    }

    struct Probe { players: Option<u64>, raw: Vec<u8> }    // raw = the reply to cache

The three roles of a protocol family map onto it directly: the **sleeping-status
responder** is `sleeping_reply`, the **join detector** is `classify` (plus `refuse_join`), and the
**idle probe** is `probe`.

`Shared`, `State`, `monitor`, `on_join`, the limiter and freeze mode keep working on `Probe` and
`Classified` and never see protocol bytes. The Minecraft code moves behind the trait unchanged.

### Where the code in main.rs goes

| Today in `main.rs` | Goes to |
| --- | --- |
| varint and packet helpers, `string_packet`, `read_packet`, `send` | `proto/minecraft.rs` |
| `ping_backend`, `players_online` | `proto/minecraft.rs` as `probe` |
| `sleeping_status` (the JSON edit), default kick texts, `send_kick` | `proto/minecraft.rs` as `sleeping_reply` / `refuse_join` |
| handshake parsing in `handle`, `host_of`, the split between status and login | `proto/minecraft.rs` as `classify` |
| `status_loop` (status then ping/pong on one connection) | stays Minecraft-specific; `sleeping_reply` has to be callable repeatedly on one connection |
| `Config`, `State`, `Phase`, `Shared` (`became_up`, `on_join`, `run`, `resume`, `check_frozen`) | `core.rs`, unchanged apart from `cache: Option<String>` becoming bytes |
| `monitor` | `core.rs`; calls `protocol.probe()` instead of `ping_backend` |
| `Router`, accept loop, `forward`, connection counting | `listener/tcp.rs` |
| `check`, `check_one`, `main` | stay in `main.rs` |

`Config` gains `protocol = "minecraft"` (the default, so existing configs keep working) and the
Minecraft-only keys (`version_name`, `protocol` number, `max_players`, `properties_file`) become
that family's settings. The integer key `protocol` clashes with the new meaning and would have to
be renamed with an alias (`protocol_version`), which is the one unavoidable config break.

## Generic TCP mode

`protocol = "tcp"`: for anything dormant does not understand (a web app, a game with a TCP
protocol, SSH to a box that should sleep).

- **Wake on connect.** `classify` returns `Join` for every connection without reading anything.
  There is no status/join distinction, so port scanners wake the server; the rate limits and a
  source-address allowlist (CIDRs, new) are the only protection, and the documentation has to say
  so plainly.
- **Hold instead of kick.** There is no way to tell an unknown client "come back in 60 seconds",
  so the connection is held open while the server boots and then spliced to the backend. This is
  new behaviour for the core: today a join during `Starting` is answered and closed. It needs a
  `Notify` (or watch channel) in `Shared` that `became_up` fires, a cap on held connections, and a
  hold timeout (`wake_timeout_secs` fits). Clients with short connect timeouts will give up
  before a slow server is up; that cannot be fixed here.
- **Up check.** `probe` is "does a TCP connect succeed". A port that accepts before the
  application is ready will be marked up too early; an optional `ready_cmd` (shell command, exit
  status 0 = ready) covers that.
- **Idle = no connections.** The existing `conns` counter is the whole idle signal. Protocols with
  long-lived idle connections (a database pool) never look idle; that is a documented limit, with
  an optional `idle_cmd` as the escape hatch.
- **No hostname routing**: one listener port per server. So `listen` has to become allowed per
  server, and the accept loop becomes one task per listener. This is the largest structural
  change in the TCP part.
- Freeze mode works as it is and is a good match for hold: resume, wait for the port, splice.

Holding would also be a welcome option for Minecraft itself (lazymc has it), and falls out of this
work for free.

## Generic UDP mode

`protocol = "udp"`: most non-Minecraft game servers.

- **Wake on first datagram.** A datagram from a source with no session, while the server is off,
  is a wake request. The same scanner caveat applies, more strongly: UDP sources are trivially
  spoofed, so the per-address limit means little and the global limit is the one that counts.
- **Sessions by source address.** dormant keeps a map from client address to a dedicated upstream
  socket (connected to the backend) and relays both ways, as any UDP proxy does. A session ends
  after `session_timeout_secs` without datagrams in either direction. Memory needs a hard cap on
  sessions.
- **Nothing is held.** Datagrams that arrive while the server boots are dropped (or the last one
  per client is kept and replayed; it rarely helps, because game handshakes time out in a few
  seconds). The player has to retry. Without a family-specific responder they see only "no
  response", which is the real weakness of plain UDP mode.
- **Idle = no sessions.** Server-browser queries from the internet create sessions too, so a
  public server may never look idle. A family responder (below) fixes that by classifying queries
  as `Status`, which do not create sessions.
- **Up check**: UDP has no connect. Without a family probe the choices are `ready_cmd` or a fixed
  `wake_secs_default` delay. Neither is good.
- The backend sees dormant's address as the source of every packet. Games that ban or rate limit
  by address, or that use the source address for NAT punch-through, will misbehave.
- New tokio feature needed: none beyond `net` (already enabled). A UDP relay at game-server packet
  rates through one tokio task per session is fine for a handful of players, not for hundreds.

## Per-family status responders

Plain TCP/UDP modes wake and sleep a server but show nothing useful while it is asleep. A family
responder restores what the Minecraft code does today: the server looks alive in the browser, and
only a real join wakes it.

**Steam A2S (Source query protocol; many Steam games).**
- `classify`: datagrams starting `FF FF FF FF` are queries (`A2S_INFO` 0x54, `A2S_PLAYER` 0x55,
  `A2S_RULES` 0x56) and are `Status`. Everything else on the game port is `Join`/`Other`.
- `sleeping_reply`: replay the cached `A2S_INFO` answer with the player count set to 0 and the
  server name replaced or suffixed ("(asleep, join to wake)"). The reply is a flat binary record
  with NUL-terminated strings, so editing the name means re-encoding it; the rest can stay opaque.
  The challenge mechanism (the server answers `0x41` plus a challenge, the client repeats the
  query with it) has to be implemented for clients to accept the answer.
- `probe`: send `A2S_INFO` to the backend; players = the count in the reply. This is a real idle
  probe, as good as Minecraft's.
- Complication: many games use a separate query port. Then the query responder and the game-port
  relay are two listeners for one server, and the wake trigger is the game port.
- No way to show a "starting, try again" message to a joining client; they time out.

**Bedrock (RakNet).**
- `classify`: `0x01`/`0x02` unconnected ping is `Status`; `0x05` open connection request 1 is
  `Join`.
- `sleeping_reply`: an unconnected pong (`0x1c`) echoing the ping time, with the server id string
  (`MCPE;motd;protocol;version;players;max;guid;...`). The cached string from the real server is
  edited in two fields (MOTD and player count), which is the same opaque-passthrough approach as
  for Java.
- `probe`: unconnected ping to the backend, players from the pong's id string.
- Routing by hostname is impossible (the name is not in the packets): one port per server.
- A join during boot can only be dropped; the client shows "unable to connect". Holding is not
  possible, because RakNet's own handshake would time out.

Both are a few hundred lines each and would live in `proto/a2s.rs` and `proto/raknet.rs`.

## Effort, honestly

Estimates for someone who knows the code base, including tests and documentation. They are
guesses; the UDP parts are the least certain.

| Step | Size | Notes |
| --- | --- | --- |
| 1. Extract `Protocol`, move Minecraft behind it, no behaviour change | 2-3 days | Mostly moving code. The harness has to pass unchanged; that is the acceptance test. `main.rs` roughly halves. |
| 2. Per-server `listen`, several listeners | 1-2 days | Config validation gets more cases (two servers on one port need a routable protocol). |
| 3. Hold mode in the core (wait for up, then splice) | 2-3 days | Needs care around freeze/resume races and caps; also usable for Minecraft. |
| 4. Generic TCP mode | 1-2 days after 2 and 3 | Small once hold exists. `ready_cmd`, address allowlist. |
| 5. Generic UDP relay with sessions | 4-6 days | A new data path with its own failure modes (session table, timeouts, caps, spoofing). Needs a UDP fake backend in the harness. |
| 6. A2S responder | 3-4 days after 5 | Challenge handling, query-port split, testing against two or three real games. |
| 7. RakNet responder | 2-3 days after 5 | Simpler packets, but needs testing against a real Bedrock server and clients on several platforms. |

Roughly three to four weeks of focused work for everything, and the code would grow from about
1,500 lines to perhaps 3,500. Steps 1 to 4 (about a week and a half) are low risk and keep the
project recognisably the same. Steps 5 to 7 turn dormant into a UDP proxy as well, which is a
different kind of program: per-packet relaying is a performance-sensitive path that the current
design (forward a TCP stream and get out of the way) never needed, and each game family brings
its own long tail of compatibility reports.

Things that would stay out of scope in any case: TLS termination, HTTP awareness, protocol
translation, load balancing, and anything that needs dormant to understand a game's play protocol.

## Recommendation

Do step 1 only when there is a second protocol to add; an abstraction with one implementation is
cost without benefit. If there is demand, the order that keeps each step useful on its own is:
hold mode for Minecraft (3), then the trait (1), per-server listeners (2) and TCP mode (4). Treat
UDP and the per-family responders as a separate decision, possibly a separate binary sharing the
core.
