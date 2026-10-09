# Weft

Open-source peer-to-peer virtual LAN for Linux, Windows and macOS, in the spirit of Hamachi and Radmin VPN.

## Features

- Networks with a name and a password, invite links, join approval, kick and ban.
- LAN games work: broadcast and multicast are forwarded across the network.
- Direct encrypted connections with NAT traversal (IPv4 and IPv6), relay fallback.
- Peer names like `nick.weft`, ping and connection type for every member.
- Use the public server, host one on your own computer, or deploy one to a VPS over SSH from the app.
- Server web panel: networks, devices, traffic.
- Native desktop app without a browser engine, in English, Russian, Spanish and Arabic.

## Install

Download a package from [Releases](https://github.com/qateralong/weft/releases/latest):

| System | Package |
|---|---|
| Windows | `weft-*-x64.msi` |
| macOS | `weft-*.pkg` |
| Debian, Ubuntu | `weft_*_amd64.deb` |
| Fedora, openSUSE | `weft-*.x86_64.rpm` |
| Arch | `packaging/aur/PKGBUILD` |

Server only: `weft-loom` packages, static `loom-*-musl` binaries or `ghcr.io/qateralong/loom` ([compose.yaml](packaging/docker/compose.yaml)).

## Components

| Name | Role |
|---|---|
| `weft-gui` | Desktop app |
| `weft` | CLI |
| `weftd` | Daemon: TUN, encryption, peer connections, built-in server |
| `loom` | Server: networks, membership, addresses, hole punching, relay, web panel |

Protocol: Noise IK over a single UDP socket; control channel over TLS.

## License

[AGPL-3.0](LICENSE)
