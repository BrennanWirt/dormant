# dormant with Crafty Controller

[Crafty Controller](https://craftycontrol.com/) 4 is a web panel that runs Minecraft servers.
dormant can sit in front of a server that Crafty manages and start / stop it through Crafty's API.
`crafty-ctl` in this directory is the small wrapper script that does that (needs `curl` and `python3`).

1. In Crafty, give the server a port other than the public one (for example 25566) and switch its
   auto-start off: dormant decides when it runs.
2. Create an API token in Crafty for a user that may start and stop the server, and save it:

       install -d -m 700 /etc/dormant
       echo 'PASTE-THE-TOKEN-HERE' > /etc/dormant/crafty-token
       chmod 600 /etc/dormant/crafty-token

3. Install the wrapper and try it with the server's id (the UUID in the server's Crafty URL):

       install -m 755 crafty-ctl /usr/local/bin/crafty-ctl
       crafty-ctl 00000000-0000-4000-8000-000000000000 running; echo $?

4. Add the server to dormant's config. `--crafty-id` is a shortcut that fills in the two commands:

       dormant new survival --crafty-id 00000000-0000-4000-8000-000000000000 \
           --backend-port 25566 --hosts mc.example.com

   which writes

       start_cmd = "/usr/local/bin/crafty-ctl 00000000-0000-4000-8000-000000000000 start"
       stop_cmd  = "/usr/local/bin/crafty-ctl 00000000-0000-4000-8000-000000000000 stop"

   If the script lives elsewhere, or the panel is not on `https://localhost:8443`, write the commands
   yourself with `--start-cmd` / `--stop-cmd`, for example
   `--start-cmd "CRAFTY_URL=https://panel.example.com /opt/bin/crafty-ctl <id> start"`.

Freeze mode: Crafty starts the Java process as its own child, in Crafty's process group. dormant
handles that case (it signals the Java process and its children, never Crafty itself). Pass
`--mode freeze --properties-file <path to that server's server.properties>`.
