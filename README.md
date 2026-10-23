# Packet Lab

An experimental Linux Quick Share client based on [Packet](https://github.com/nozwock/packet), with Bluetooth discovery and a manual local-address fallback for networks where multicast discovery is unavailable.

Packet Lab has its own executable, application ID, settings, device name and log path, so it can run alongside Packet. It keeps the existing pairing, consent and encrypted transfer protocol.

## Run the native test bundle

Extract the Linux x86_64 archive and run `./packet-lab` from its folder. This is a **native test bundle**, not a universal AppImage: it needs GTK 4.22 or newer, libadwaita 1.9 or newer, and compatible system libraries. See `BUILD-INFO.txt` in the bundle for the exact build environment. The first bundle is built on Arch Linux with glibc 2.44.

## Send without entering an IP address

Keep Bluetooth enabled and open Quick Share's receive screen on the phone. In **Send To…**, select its Bluetooth card, compare the pairing code and accept on the phone. The address shown below each Bluetooth name helps distinguish devices with the same name.

Packet Lab establishes the encrypted Nearby session over Bluetooth Classic/RFCOMM. For large payloads it follows the receiver's Wi-Fi LAN upgrade negotiation, retaining the encryption keys and sequence numbers. Small payloads below Nearby's 1 MB threshold can remain on Bluetooth. The Nearby phone was verified with a 16 MiB Bluetooth-to-Wi-Fi transfer on the school network with both VPNs enabled; this is not a guarantee for other network policies.

This first implementation supports encoded Nearby Bluetooth names and devices exposing Nearby's SDP service. A first connection may need pairing/service discovery in the system Bluetooth settings. Scanning remote BLE GATT slots, Wi-Fi Direct, and Internet relaying are not implemented. Wi-Fi LAN handover still requires direct local TCP reachability. See [Bluetooth validation](docs/BLUETOOTH-WIFI.md).

## Send when discovery fails

1. Select a file or text, then choose **Send To…**.
2. Choose **Connect by IP…**.
3. Enter the other device's confirmed local IPv4 address and Quick Share port, such as `192.168.1.2:53601`. The port is an example, not a guaranteed Android default.
4. Keep the other device's receive screen open. Compare the pairing code and accept the request.

Only private/link-local IPv4 addresses with an explicit port are accepted. A connection failure is shown as Failed; the existing retry action can try again. No subnet scanning or arbitrary TXT-address fallback is added.

## Thunar right-click sharing

The native bundle includes `packet-lab-send` (Python 3 with PyGObject/Gio required). In Thunar's **Edit → Configure custom actions**, add an action named **Send with Packet Lab**. Use the absolute path to the bundle helper followed by unquoted `%F`, for example:

```text
"/path/to/bundle/packet-lab-send" %F
```

Set the file pattern to `*`, enable all file categories, and leave directories disabled. Multi-selection and spaces in filenames are supported. The helper starts Packet Lab if needed and opens its recipient picker; it never chooses a recipient automatically. Keep the bundle in the same location or update the action's command after moving it.

VPNs can remain enabled, but successful local sharing depends on their routing/firewall policy. Local-interface sockets travel outside the VPN tunnel. For a peer that needs an explicitly selected Linux interface, start with `RQS_LAN_ROUTE='192.168.1.2%wlan0' ./packet-lab`. This applies only to that peer; other traffic keeps system routing. It does not override a VPN firewall or support a requirement that the file itself travel inside the VPN tunnel.

## Build and test

Existing native build dependencies include Rust/Cargo, Meson/Ninja, Blueprint compiler, GTK4/libadwaita development libraries, gettext, D-Bus development libraries and protoc.

```sh
meson setup _build -Dprofile=default --prefix=/usr
ninja -C _build data/resources/resources.gresource src/ui-resources.gresource
glib-compile-schemas _build/data
cargo test --locked
cargo test --locked -p rqs_lib --lib
./scripts/package-native.sh
```

The package script creates a native archive and SHA-256 checksum in `dist/`. It does not install into system directories or modify the original Packet application. Local schema loading and resources are supplied by the bundle launcher.

The portable folder does not install desktop or D-Bus activation entries. Portal host registration can warn that the app is not installed. Auto-start, background permissions and file-manager integration are not validated for this bundle; launch it directly and keep it open for testing.

## Receiving behind a firewall

The default receiver uses a dynamically allocated TCP port. Discovery may work while incoming file transfers time out if the firewall only allows a fixed port. In Preferences, enable the static port and choose a TCP port already permitted on the local network. For example, an existing Packet setup may permit 9300. Only one app can listen on the same port: use different permitted ports when running Packet and Packet Lab together. Keep the VPN enabled; changing this application setting does not change VPN or firewall rules.

## Status

This is an alpha experiment. Compatibility with every school network, Wi-Fi Direct and mid-transfer resume are not promised. A manual address can help when multicast discovery is unavailable; it cannot fix network isolation that also blocks TCP.

Outgoing transfers remain in progress after the final payload chunk until the receiver closes the transfer connection. Receiver errors and a 30-second finalization timeout fail the transfer. This follows the receiver-driven completion ordering in Google Nearby's `OutgoingShareSession::DelayComplete`. File introductions include distinct nonzero attachment IDs for Google's receiver. A plain-text file and the original failing `.ps1` were successfully received by Windows Quick Share after these fixes; see `docs/REMOTE-COMPLETION.md` for evidence and limits.

Source attribution and licenses are in [UPSTREAM.md](UPSTREAM.md) and [LICENSE](LICENSE). No proprietary Nearby binaries, private device logs or school credentials are included. This local build has not been uploaded to GitHub or published to Flathub.
