# Weft

Open-source peer-to-peer virtual LAN for Linux, Windows and macOS, in the spirit of Hamachi and Radmin VPN.

> Status: design stage, no code yet.

## Goals

- Join friends' machines into one virtual network with a name and a password.
- LAN games work: broadcast discovery is forwarded across the network.
- Direct encrypted connections between peers, with NAT traversal and relay fallback.
- Fully self-hosted: run the coordination server at home or on a VPS.

## Design decisions

| Area | Decision |
|---|---|
| Network layer | L3 (TUN): Wintun on Windows, utun on macOS, `/dev/net/tun` on Linux; broadcast emulation |
| Language | Rust |
| Protocol | Custom, based on Noise IK, single UDP socket for data, hole punching and relay |
| Server | Self-hosted coordination server with STUN and relay |
| License | AGPL-3.0 |

## Components

| Name | Role |
|---|---|
| `weft` | CLI |
| `weftd` | Privileged daemon: TUN, crypto, peer connections |
| loom | Coordination server: networks, membership, address allocation, key and endpoint exchange |
| shuttle | Relay for peers that cannot connect directly |

## License

[AGPL-3.0](LICENSE)
