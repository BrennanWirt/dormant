# Changelog

All notable changes to this project are listed here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and the project uses
[Semantic Versioning](https://semver.org/).

## 0.1.0 - unreleased

Initial public release.

- Sleep proxy for Minecraft Java servers: answers status pings while the server is off, starts it
  on a login, stops it when idle.
- Opaque status passthrough: the sleeping reply is the server's last real status with only the
  MOTD and player count replaced; the handshake is replayed unchanged (Forge 1.12.2 compatible).
- Several servers on one port, routed by hostname (`[[server]]`, `default`, `unknown_motd`).
- Freeze mode (`sleep_mode = "freeze"`) with safety checks, and adoption of a frozen server after a
  restart.
- Measured wake time (`{wake_secs}`, `{secs}`) in MOTDs and kick texts.
- Wake allowlist (`wake_names`) and wake rate limits per source address and per server
  (`wake_limit_per_ip`, `wake_limit_global`, `wake_limit_window_secs`, `kick_rate_limited`).
- `dormant --check`, `dormant list`, `dormant new`.
- Examples for systemd, Docker and Crafty Controller; end-to-end harness with a fake backend.
