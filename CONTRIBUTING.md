# Contributing

Bug reports and small, focused pull requests are welcome.

## Reporting a problem

Please include:

- the output of `dormant --version` and of `dormant --check <config>` (remove anything private),
- the Minecraft version and server type (vanilla, Paper, Forge, NeoForge, Fabric, ...),
- the relevant log lines; dormant logs `key=value` lines to stderr.

For a server whose sleeping server-list entry looks wrong, the cached status file (`cache_file`) is
the most useful attachment.

## Changing the code

    cargo fmt
    cargo clippy -- -D warnings
    cargo test
    cargo build && harness/run.sh      # end-to-end test against fake backends (Linux, python3)

CI runs the same four steps.

What the project tries to stay:

- **Small.** A few files, four dependencies (tokio, serde, serde_json, toml). A new dependency needs
  a good reason.
- **No `unsafe`**, no libc calls. Process handling goes through `/proc` and `sh`.
- **Protocol-agnostic past the handshake.** dormant reads the handshake and, while the server is
  off, the login start. It never parses the status reply into typed structures and never touches
  the play protocol. That is what keeps modded servers working.
- **Config is the only source of commands.** Nothing received from the network may end up in a
  shell command or a file name.

New behaviour should come with a unit test, and with a check in `harness/run.sh` when it changes
what a client sees. Every config key must be documented in the README table and in
`examples/dormant.toml`.

[GENERALIZE.md](GENERALIZE.md) sketches how dormant could grow beyond Minecraft Java; read it before
proposing support for another game.

## License

By contributing you agree that your contribution is licensed under MIT OR Apache-2.0, like the
rest of the project.
