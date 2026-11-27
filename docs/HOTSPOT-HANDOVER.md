# Bluetooth to Wi-Fi handover

Packet Lab uses Bluetooth discovery and an authenticated Nearby handshake before
upgrading to Wi-Fi LAN, a private Wi-Fi Hotspot, or Wi-Fi Direct. Bluetooth remains
the fallback when a compatible high-speed channel cannot be established.

## Requirements and behavior

The Linux helpers require NetworkManager, wpa_supplicant, iw, ip, dnsmasq,
system Python with PyGObject, and polkit. The adapter must support a separate AP
or P2P interface concurrently with its existing station connection. Concurrent AP
operation uses the station channel; actual support depends on the driver.

Incoming transfers can offer a private Hotspot. Outgoing transfers can join an
Android legacy Wi-Fi Direct group using credentials received over the authenticated
session. The P2P client waits for NetworkManager activation and a validated address
before switching sockets. Temporary profiles and groups are removed on cleanup;
the existing default route and DNS are retained. Joining an ordinary phone-hosted
Hotspot while preserving the station connection is currently unsupported.

The negotiated legacy Wi-Fi Direct mode can use WPA2 link encryption without the
additional UKEY2 record wrapper, only after the authenticated offer and channel
switch. Bluetooth, LAN and unnegotiated paths retain UKEY2. Decode failures never
trigger an automatic change to an unencrypted record format.

## Optional installation to avoid repeated system passwords

By default, network setup requires administrator authorization for each helper
invocation. Install trusted helper copies once from the native bundle:

```sh
sudo /usr/bin/python3 -I /absolute/path/to/bundle/share/packet-lab/install-network-helper.py --user YOUR_LOGIN
```

The installer places two fixed, root-owned helpers in
`/usr/lib/packet-lab-network/` and installs dedicated polkit actions. Its rule grants
the selected account access to those two operations, including processes running
in the background or over SSH under that account. It grants no general Python,
shell or sudo exemption. Quick Share receive consent remains required.

The app rejects installed helpers with unsafe ownership, write permissions or
symlinked ancestors. Without a safe installation, it uses administrator-prompt
authorization. Rerun the installer after helper updates. To remove the installation:

```sh
sudo /usr/bin/python3 -I /absolute/path/to/bundle/share/packet-lab/install-network-helper.py --uninstall
```

Bluetooth pairing is separate from system network authorization. A user-confirmed
saved pairing can avoid repeated Bluetooth confirmations; a MAC address alone
does not authenticate a device. Packet Lab does not silently accept pairing codes.

## Android BLE reception

Packet Lab advertises a BLE L2CAP receive channel by default. Bluetooth
reception requires connectable extended advertising on the Linux adapter and
an Android Quick Share build that supports the advertised LE channel. Wi-Fi LAN
reception remains available. Set `RQS_BLE_L2CAP=0` before launching Packet Lab to
use Classic Bluetooth reception for compatibility. The Classic discovery path
can trigger a system pairing prompt. Outgoing Bluetooth transfers use RFCOMM.

On the tested Samsung, two 64 KiB transfers completed in both directions with
matching SHA-256 while phone-to-Linux LAN TCP was unavailable. Neither transfer
required manually accepting or saving a Bluetooth pairing. A separate GATT
pairing prompt still appeared during reception and could be left unanswered.
Both files travelled over Bluetooth in this test.

## Validation and limits

Android hardware tests completed bidirectional 16 MiB 5 GHz Wi-Fi Direct
transfers (both PC-to-phone and phone-to-PC via BLE L2CAP + dynamic role-switch
`UPGRADE_PATH_REQUEST`) with ordinary LAN excluded, matching SHA-256 digests, no
saved Bluetooth bond, and automatic P2P interface cleanup between consecutive
transfers. Phone-to-PC Hotspot transfers also completed with matching hashes.
Both VPNs and the active station Wi-Fi connection remained enabled in these
tests; different VPN routing policies or drivers without multi-channel P2P
concurrency can still limit local channels.

Saved pairing reduced observed Bluetooth setup from roughly 11 seconds to under
one second in a warm session. This is not a cold-start or sustained-throughput
benchmark. Reboot persistence and compatibility with other devices remain
unverified. Private device logs and identifiers are intentionally excluded from
this publication.
