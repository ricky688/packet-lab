# Changelog

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
