#!/usr/bin/env python3
"""Fake Minecraft Java backend for dormant's harness.

Answers a status request with a Forge-1.12.2-shaped reply (description as an object, a `modinfo`
mod list, 0 players online), answers ping with pong, and answers any login with the disconnect
text "backend LABEL says hi: NAME", so a test can prove that the handshake and the login packet
were replayed to the backend.

usage: fakemc.py PORT [LABEL] [--pidfile FILE]
"""
import json
import os
import socket
import sys
import threading


def varint(n):
    out = b""
    while True:
        x = n & 0x7F
        n >>= 7
        out += bytes([x | (0x80 if n else 0)])
        if not n:
            return out


def string(s):
    s = s.encode()
    return varint(len(s)) + s


def packet(body):
    return varint(len(body)) + body


def take_varint(buf):
    n = i = 0
    while True:
        x = buf[i]
        n |= (x & 0x7F) << (7 * i)
        i += 1
        if not x & 0x80:
            return n, buf[i:]


def recv_exact(conn, n):
    data = b""
    while len(data) < n:
        chunk = conn.recv(n - len(data))
        if not chunk:
            raise EOFError
        data += chunk
    return data


def recv_packet(conn):
    n = i = 0
    while True:
        x = recv_exact(conn, 1)[0]
        n |= (x & 0x7F) << (7 * i)
        i += 1
        if not x & 0x80:
            return recv_exact(conn, n)


def main():
    args = sys.argv[1:]
    pidfile = None
    if "--pidfile" in args:
        i = args.index("--pidfile")
        pidfile = args[i + 1]
        del args[i:i + 2]
    port = int(args[0])
    label = args[1] if len(args) > 1 else "FAKE"
    status = {
        "description": {"text": "REAL " + label},
        "players": {"max": 20, "online": 0},
        "version": {"name": "1.12.2", "protocol": 340},
        "modinfo": {"type": "FML", "modList": [{"modid": "examplemod", "version": "1.0"}]},
    }
    status_body = b"\x00" + string(json.dumps(status))

    def handle(conn):
        try:
            conn.settimeout(10)
            hs = recv_packet(conn)
            _, hs = take_varint(hs)  # packet id
            _, hs = take_varint(hs)  # protocol version
            n, hs = take_varint(hs)  # server address
            hs = hs[n + 2:]  # skip the address and the port
            next_state, _ = take_varint(hs)
            if next_state == 1:
                while True:
                    req = recv_packet(conn)
                    if req[0] == 0:
                        conn.sendall(packet(status_body))
                    elif req[0] == 1:
                        conn.sendall(packet(req))
                        return
            else:
                login = recv_packet(conn)
                _, login = take_varint(login)
                n, login = take_varint(login)
                name = login[:n].decode()
                text = "backend %s says hi: %s" % (label, name)
                conn.sendall(packet(b"\x00" + string(json.dumps({"text": text}))))
        except Exception:
            pass
        finally:
            conn.close()

    srv = socket.socket()
    srv.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
    srv.bind(("127.0.0.1", port))
    srv.listen(50)
    if pidfile:
        with open(pidfile, "w") as f:
            f.write(str(os.getpid()))
    while True:
        conn, _ = srv.accept()
        threading.Thread(target=handle, args=(conn,), daemon=True).start()


if __name__ == "__main__":
    main()
