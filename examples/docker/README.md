# dormant with a Docker container

dormant runs on the host and starts / stops an existing container by name.

    docker compose up --no-start        # create the container "mc" without starting it
    docker start mc                     # start it once so that dormant can cache its real status
    mkdir -p /var/lib/dormant
    dormant --check ./dormant.toml      # should print the backend's status
    dormant ./dormant.toml              # or install ../systemd/dormant.service

The user dormant runs as must be allowed to use `docker` (root, or a member of the `docker` group).

Notes:

- Publish the container's port on `127.0.0.1` only, as in `docker-compose.yml`, so that players
  cannot connect past dormant.
- The server sees every connection coming from the Docker bridge, not from the player's address,
  so IP bans inside the server do not work. dormant has no PROXY protocol support.
- Freeze mode: the owner of the published port is Docker's proxy, not Java, so the default process
  lookup finds the wrong process. Stay with the default `sleep_mode = "stop"`, or set `pid_pattern`
  to something that matches only the Java process and verify it with `dormant --check`.

This example has not been exercised by the project's automated tests; treat it as a starting point.
