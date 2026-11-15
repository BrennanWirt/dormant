#!/usr/bin/env python3
"""Minimal Minecraft Java client for dormant's harness (and for poking at a real setup by hand).

    mcclient.py ping  PORT [HOST] [--connect ADDR]   server-list ping; prints one line of JSON
    mcclient.py login PORT NAME [HOST] [--connect ADDR]
                                                     login attempt; prints the disconnect text

HOST is the server address put into the handshake (what a player would type; default
"localhost"). Like a Forge 1.12.2 client, the address is sent with a "\\0FML\\0" suffix. The TCP
connection goes to ADDR (default 127.0.0.1). The login is unencrypted, so it can never really join
an online-mode server; it only shows what the server (or dormant) answers.
"""
import json
import socket
import struct
import sys


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


def recv_exact(conn, n):
    data = b""
    while len(data) < n:
        chunk = conn.recv(n - len(data))
        if not chunk:
            raise EOFError("connection closed")
        data += chunk
    return data


def recv_varint(conn):
    n = i = 0
    while True:
        x = recv_exact(conn, 1)[0]
        n |= (x & 0x7F) << (7 * i)
        i += 1
        if not x & 0x80:
            return n


def take_varint(buf):
    n = i = 0
    while True:
        x = buf[i]
        n |= (x & 0x7F) << (7 * i)
        i += 1
        if not x & 0x80:
            return n, buf[i:]


def recv_string_packet(conn):
    """Read one packet of the shape `id, string` and return (id, string)."""
    body = recv_exact(conn, recv_varint(conn))
    n, rest = take_varint(body[1:])
    return body[0], rest[:n].decode()


def handshake(host, port, next_state):
    return packet(b"\x00" + varint(340) + string(host + "\0FML\0") + struct.pack(">H", port) + varint(next_state))


def main():
    args = sys.argv[1:]
    connect = "127.0.0.1"
    if "--connect" in args:
        i = args.index("--connect")
        connect = args[i + 1]
        del args[i:i + 2]
    if len(args) < 2 or args[0] not in ("ping", "login") or (args[0] == "login" and len(args) < 3):
        sys.exit(__doc__)
    port = int(args[1])
    conn = socket.create_connection((connect, port), 5)
    conn.settimeout(15)
    if args[0] == "ping":
        host = args[2] if len(args) > 2 else "localhost"
        conn.sendall(handshake(host, port, 1) + packet(b"\x00"))
        _, text = recv_string_packet(conn)
        status = json.loads(text)
        conn.sendall(packet(b"\x01" + struct.pack(">q", 12345)))
        pong = recv_exact(conn, recv_varint(conn))
        description = status.get("description")
        print(json.dumps({
            "motd": description.get("text") if isinstance(description, dict) else description,
            "players": status.get("players"),
            "version": status.get("version"),
            "modinfo": "modinfo" in status or "forgeData" in status,
            "bytes": len(text),
            "pong": pong[0] == 1 and struct.unpack(">q", pong[1:9])[0] == 12345,
        }, ensure_ascii=False))
    else:
        host = args[3] if len(args) > 3 else "localhost"
        conn.sendall(handshake(host, port, 2) + packet(b"\x00" + string(args[2])))
        _, text = recv_string_packet(conn)
        print("disconnect:", json.loads(text)["text"].replace("\n", " | "))


if __name__ == "__main__":
    main()
