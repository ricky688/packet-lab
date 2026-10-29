# Packet Lab TODO

## Priority: Quick Share stability

Stabilize Quick Share before starting AirDrop integration. Preserve existing
Wi-Fi/VPN connections and explicit receive confirmation.

- [x] Reuse direct Wi-Fi preparation within a transfer and remember validated
      LAN rejection/timeouts per peer for 30 seconds, cleared on service restart.
- [ ] Measure cold/warm connection and Wi-Fi Direct handover latency on hardware
      with VPN enabled; exclude time spent waiting for user consent.
- [ ] Verify the latest AppImage on the friend's Pop!_OS PC: PC-to-PC sending,
      discovery of an unpaired Samsung phone, and PC-to-phone sending.
- [ ] Confirm the fixes for `br-connection-busy`, repeated GATT probes, early
      ACCEPT responses, and optional BLE advertising stalls on real devices.
- [ ] Check cold startup, repeated transfers, visibility changes, and recovery
      after Bluetooth restarts or network changes, without duplicate recipients.
- [ ] Verify large-file transfers and SHA-256 integrity in both directions.
- [ ] Retest LAN, isolated school Wi-Fi, and no-shared-LAN scenarios with VPNs
      enabled. Confirm the actual transport: LAN, Wi-Fi Direct/hotspot, or
      Bluetooth fallback; do not infer it from speed alone.
- [ ] Verify AppImage drag-and-drop, tray activation, and Nemo integration on
      tester machines. Keep build identification and useful failure logs available.

## Deferred: native AirDrop integration

- [ ] Evaluate Owly (formerly omdrop-owl) as a separate AirDrop backend while
      retaining the current Quick Share backend and a shared Packet Lab UI.
      Upstream: https://github.com/t4t5/owly
- [ ] Recheck upstream APIs, licensing, hardware support, and system-service
      installation requirements before implementation; reuse the backend rather
      than reimplementing AWDL.
- [ ] Test iPhone/Mac discovery and bidirectional transfers without installing
      a companion app on the Apple device, retaining receive confirmation.
- [ ] Validate hardware and VPN coexistence before enabling the feature. The
      current PC reports Intel PCI ID `8086:51f1`; as checked on 2026-10-29,
      upstream lists an AX211 with this ID in exclusive mode, which disconnects
      station Wi-Fi. Shared-mode operation on this PC remains unverified.
- [ ] Keep AirDrop optional and avoid switching radio modes automatically when
      doing so would interrupt the user's Wi-Fi/VPN connection.
