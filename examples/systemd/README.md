# dormant with plain systemd units

The server is an ordinary systemd unit; dormant starts and stops it with `systemctl`.

1. Install the binary as `/usr/local/bin/dormant`.
2. Copy `minecraft.service` and `dormant.service` to `/etc/systemd/system/` and adjust the paths,
   user and memory settings in `minecraft.service`.
3. In the server's `server.properties` set `server-port=25566` and `server-ip=127.0.0.1`.
4. Copy `../dormant.toml` to `/etc/dormant/dormant.toml`. Its commands already read

       start_cmd = "systemctl start --no-block minecraft"
       stop_cmd  = "systemctl stop --no-block minecraft"

   `--no-block` makes the commands return at once; dormant watches the server itself.
5. Start the server once so that dormant can cache its real status reply, then enable dormant:

       systemctl daemon-reload
       systemctl start minecraft
       dormant --check /etc/dormant/dormant.toml
       systemctl enable --now dormant

Do not `systemctl enable minecraft`: dormant decides when it runs. With the default settings the
server stops after 15 minutes without players and starts again when someone tries to join.

For several servers, use a template unit (`minecraft@.service` with
`WorkingDirectory=/srv/minecraft/%i`) and `../multi-server.toml`.
