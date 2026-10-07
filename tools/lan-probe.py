#!/usr/bin/env python3
"""Sends or receives LAN game style discovery packets: limited broadcast and the Minecraft multicast group.

    lan-probe.py listen [seconds]   prints "<source> <kind>" for every packet until both kinds arrive
    lan-probe.py send [count]       sends both kinds every half second without binding to an interface
"""
import socket
import struct
import sys
import time

PORT = 4445
GROUP = "224.0.2.60"


def listen(seconds):
    sock = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
    sock.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
    sock.bind(("", PORT))
    membership = struct.pack("4s4s", socket.inet_aton(GROUP), socket.inet_aton("0.0.0.0"))
    sock.setsockopt(socket.IPPROTO_IP, socket.IP_ADD_MEMBERSHIP, membership)
    sock.settimeout(seconds)
    seen = set()
    try:
        while len(seen) < 2:
            data, (source, _) = sock.recvfrom(2048)
            kind = data.decode(errors="replace")
            print(source, kind, flush=True)
            seen.add(kind)
    except socket.timeout:
        pass
    return 0 if len(seen) == 2 else 1


def send(count):
    sock = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
    sock.setsockopt(socket.SOL_SOCKET, socket.SO_BROADCAST, 1)
    for _ in range(count):
        sock.sendto(b"broadcast", ("255.255.255.255", PORT))
        sock.sendto(b"multicast", (GROUP, PORT))
        time.sleep(0.5)
    return 0


if __name__ == "__main__":
    mode = sys.argv[1] if len(sys.argv) > 1 else "listen"
    value = int(sys.argv[2]) if len(sys.argv) > 2 else 10
    sys.exit(listen(value) if mode == "listen" else send(value))
