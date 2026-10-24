# Upstream and licensing

Packet Lab is a modified snapshot of Packet 0.7.3 by nozwock, at commit
1e9fc400020b6fcdbae6ed586872a8d17b6fde26:
https://github.com/nozwock/packet

The vendored rquickshare core is based on commit
c9ac93f5584a4265e8da679ad689df5be1e80c6b from:
https://github.com/nozwock/rquickshare

Both original projects use GPL-3.0. Original code, artwork and translation
credits are retained. See LICENSE and subprojects/rqs_lib/LICENSE.

Changes in this experiment:

- Independent Packet Lab application identity, configuration and cache.
- Manual local IPv4/port connection using the existing send and pairing path.
- Explicit peer-scoped Linux interface selection shared by discovery and send.
- Bounded connection setup, cancellation and visible failure reporting.
- Automatic physical LAN interface selection and VPN/virtual interface filtering.
- Bi-directional Wi-Fi Direct (P2P) bandwidth upgrade and AP-isolation automatic fallback.
- BLE 0xFEF3 discovery and remote GATT slot reading.
- Dynamic transport medium status badges, real-time speedometer & ETA display, and device type icons.
- Command-line direct sending (`--send` / `-s`).

## Citations & Related Projects

- **[Packet](https://github.com/nozwock/packet)** by nozwock (GPL-3.0) — The primary base for Packet Lab's GTK4/Libadwaita frontend and Nearby Share integration.
- **[rquickshare](https://github.com/Martichou/rquickshare)** by Martichou / nozwock (GPL-3.0) — Core Rust protocol implementation for Quick Share / Nearby Connections.
- **[Google Nearby](https://github.com/google/nearby)** (Apache-2.0) — Official Google Nearby Connections specifications and reference protocol implementation.
- **[NearDrop](https://github.com/grishka/NearDrop)** by grishka (GPL-3.0) — Reference Nearby Share implementation for macOS.

No proprietary third-party binaries or code are incorporated into this repository.
