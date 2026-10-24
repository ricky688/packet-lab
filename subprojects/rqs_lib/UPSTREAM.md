# Upstream core

Based on https://github.com/nozwock/rquickshare/tree/c9ac93f5584a4265e8da679ad689df5be1e80c6b/core_lib
(the revision pinned by Packet 0.7.3). GPL-3.0; see LICENSE.

Local changes add an explicit, peer-scoped Linux interface route, bounded
connection attempts, and shared connection handling for discovery and sending.
Only Rust build inputs and the license are retained. No proprietary binaries
or private device logs are included.
