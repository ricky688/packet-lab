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
- Tests and a native Linux test-bundle launcher.

No Nearby proprietary code or binary is incorporated. Private reverse
engineering artifacts and device logs remain outside this source repository.
