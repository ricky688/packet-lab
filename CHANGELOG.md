# Changelog

## Bluetooth / Wi-Fi experiment — 2026-10-23

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
