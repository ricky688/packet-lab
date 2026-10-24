# Changelog

## Universal multi-device discovery, zero-config LAN filtering, and Wi-Fi P2P BWU handover — 2026-10-24

- Implemented automatic physical LAN interface detection (`Connector::is_physical_lan_interface` and `Connector::find_interface_for_peer`) to filter out virtual/VPN/Docker/Bluetooth-bridge interfaces (`vpn`, `tun*`, `tailscale*`, `docker*`, `btnap*`, `virbr*`, `p2p-dev-*`) in `MDnsServer`, `MDnsDiscovery`, and `Connector` without requiring a single-peer `RQS_LAN_ROUTE` override.
- Implemented sender-side BLE `0xFEF3` (`0000fef3-0000-1000-8000-00805f9b34fb`) extended advertisement parsing and remote GATT slot reading (`parse_receiving_payload`, `resolve_ble_fef3_device`, `read_fef3_gatt_slots`) in `rqs_lib::bluetooth` so unpaired Android phones can be discovered and resolved to their Classic RFCOMM MAC address.
- Wired `WifiP2PController::connect_for_upgrade` (`src/p2p.rs`) into `rqs_lib::bwu` (`Upgrade::handle`) via `rqs_lib::set_p2p_upgrader` to execute end-to-end `Medium::WifiDirect` and `Medium::WifiHotspot` bandwidth upgrades with automatic fallback to encrypted Bluetooth on failure.
- Implemented Receiver-Initiated `Medium::WifiDirect` offer (`WifiDirectOffer`, `set_p2p_host_provider`, `Upgrade::offer`) and automatic AP-isolation fallback (`Medium::WifiLan` `UpgradeFailure` -> automatic `Medium::WifiDirect` `UpgradePathAvailable` offer -> encrypted Bluetooth fallback).
- Added unit tests covering virtual/VPN interface filtering, BLE `0xFEF3` slot round-trip decoding, `WifiDirect`/`WifiHotspot` BWU handover, and Receiver-Initiated `Medium::WifiDirect` offer with AP-isolation fallback (36 `rqs_lib` tests + 4 `packet-lab` tests passing).

## NetworkManager Wi-Fi P2P integration — 2026-10-24

- Integrated NetworkManager `org.freedesktop.NetworkManager.Device.WifiP2P` and `WifiP2PPeer` D-Bus interfaces via `zbus` in `src/p2p.rs`.
- Implemented automatic discovery of the system's Wi-Fi P2P management device (`p2p-dev-wlo1`), supporting `start_find`, `stop_find`, and type-safe peer enumeration (`P2PPeerInfo`).
- Added structured parsing for incoming `Medium::WifiDirect` and `Medium::WifiHotspot` credentials in `rqs_lib::bwu`.
- Verified live D-Bus communication with NetworkManager on Intel Raptor Lake host without interrupting active Wi-Fi or VPN connections.

## Bandwidth upgrade negotiation and Wi-Fi LAN transfer — 2026-10-24

- Implemented `BandwidthUpgradeRetry` (frame type 12) protobuf wire format and handler in `rqs_lib`, responding to sender upgrade retries with supported mediums (`WifiHotspot`, `WifiDirect`, `WifiLan`) and explaining previously unhandled 10-second upgrade timeouts.
- Added optional `RQS_AUTO_ACCEPT` test environment variable to streamline automated integration testing of incoming transfers.
- Successfully verified end-to-end Phone-to-Linux Quick Share file transfer over local Wi-Fi LAN with active VPNs retained on both the Nearby SM-S9280 (serial Android-Device, 192.168.1.128) and Linux host (wlo1, 192.168.1.143).
- Received synthetic test file `packet-lab-test-1MiB.bin` (1,048,576 bytes) in 163 ms (~6.4 MB/s), with 100% bit-for-bit SHA-256 match:
  `3ab07235fcc8fe594e8771ab6fe19d784f83abe8f8bb7cd6a17f5384568b29c9`.
- Verified that Android Quick Share directly connected to Linux TCP port 9300 over Wi-Fi, completed UKEY2 verification (PIN 3202), and reported "已發送" (Sent).
- All 32 core unit tests passed.

## PAN receiving experiment — 2026-10-24

- Verified phone-to-Linux encrypted RFCOMM receiving of a 1 MiB synthetic file (`packet-lab-bt-fallback-1MiB (1).bin`) with matching byte count (1,048,576 bytes) and SHA-256 (`fbbab289f7f94b25736c58be46a994c441fd02552cc6022352e3d86d2fab7c83`).
- Verified that this transfer succeeded purely over encrypted Bluetooth RFCOMM (~30.6 KiB/s payload transfer rate across 34.3 seconds, 44.6 seconds total from consent); explicitly distinguishing this from a Wi-Fi LAN or PAN handover.
- Use Bluetooth-only Nearby discovery and transfer when the explicitly selected interface belongs to Bluetooth; avoid announcing PAN as Wi-Fi LAN.

- Explicitly request an automatic RFCOMM listener for the custom NearbySharing UUID.
- Resume accepted incoming transfers after a rejected LAN upgrade instead of leaving consent deferred.

- Restrict receiver mDNS advertisements to the explicitly selected local interface.
- Retain encrypted Bluetooth after a peer rejects our pending LAN upgrade, with matching-path validation and pending-session cleanup.

## Bluetooth fallback — 2026-10-24

- Report failed Wi-Fi LAN upgrades to the peer and retain the encrypted Bluetooth channel.
- Release large-payload consent after LAN failure without retaining the old handover timer.
- Keep slow Bluetooth transfers alive using smaller chunks and control-message handling; verified a 1 MiB Nearby transfer with matching hashes.

## Discovery cleanup — 2026-10-24

- Stop the previous discovery session on refresh and shut down its mDNS daemon.

## Retry reliability — 2026-10-24

- Prevent duplicate sends and retries from falsely failing an active recipient.
- Keep Bluetooth discovery running when a nearby device disappears.
- Bound BlueZ connection attempts and clean up outgoing tasks on service shutdown.
- Preserve failure IDs and show detailed connection/handover errors on recipient cards.

## Bluetooth / Wi-Fi experiment — 2026-10-23

- Publish a receiving FEF3 BLE advertisement and GATT endpoint slot, fixing Nearby discovery of the Linux receiver; complete incoming transfer verification remains pending.

- Discover Nearby Bluetooth endpoints and connect with RFCOMM without entering an IP.
- Follow receiver-initiated Wi-Fi LAN upgrade while retaining session encryption and sequence numbers; bound handover waiting and keep the TCP listener responsive.
- Distinguish same-name Bluetooth cards by address and restore adapter settings on normal application quit.
- Verified a 16 MiB Nearby phone transfer on school Wi-Fi with both VPNs enabled and matching SHA-256. See `docs/BLUETOOTH-WIFI.md` for remaining limits.

## Local fixes — 2026-10-23

- Advertise a fully qualified `.local.` hostname in mDNS SRV and address records.
- Assign distinct nonzero file attachment IDs so Google's receiver can track each attachment's payload.
- Keep outgoing transfers open until the receiver ends the connection instead of reporting success and sending a disconnect immediately after the last payload chunk.
- Parse receiver payload acknowledgements and rejection/cancellation controls, and bound finalization waiting to 30 seconds.
- Added Thunar multi-file sharing through the existing application action.

## 0.1.0-alpha.1 — 2026-10-22

- Initial local Packet Lab experimental build based on Packet 0.7.3.
- Added manual local IPv4/port connection when discovery is unavailable.
- Separated the executable, app ID, settings, cache and default device name.
- Added bounded connection setup and failed-connection reporting.
- Preserved original pairing, consent and encrypted transfer handling.
