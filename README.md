# Packet Lab

An experimental Linux Quick Share client based on [Packet](https://github.com/nozwock/packet), with Bluetooth discovery and a manual local-address fallback for networks where multicast discovery is unavailable.

Packet Lab has its own executable, application ID, settings, device name and log path, so it can run alongside Packet. It keeps the existing pairing, consent and encrypted transfer protocol.

## AppImage alpha

The x86_64 AppImage bundles the GTK/libadwaita runtime for the Ubuntu 24.04 /
glibc 2.39 baseline. Mark it executable, then launch it. Bluetooth/network system
services and drivers remain host requirements. See [AppImage instructions](docs/APPIMAGE.md)
for checksum verification, Nemo/Thunar integration, optional network helper setup
and the build recipe. Compatibility is limited to the systems actually tested.

## Run the native test bundle

Extract the Linux x86_64 archive and run `./packet-lab` from its folder. This is a **native test bundle**, not a universal AppImage: it needs GTK 4.22 or newer, libadwaita 1.9 or newer, and compatible system libraries. See `BUILD-INFO.txt` in the bundle for the exact build environment. The first bundle is built on Arch Linux with glibc 2.44.

## Send without entering an IP address

Drag one or more files from your file manager onto Packet Lab's main page to open
**Send To…**, then choose a recipient and accept the request on the receiving
device. Dropping files onto the file list adds them to the selection and opens
the same picker. Files are copied; dropping never selects a recipient automatically.
Folders and empty files are currently unsupported.

Keep Bluetooth enabled and open Quick Share's receive screen on the phone. In **Send To…**, select its Bluetooth card, compare the pairing code and accept on the phone. The address shown below each Bluetooth name helps distinguish devices with the same name.

Packet Lab establishes the encrypted Nearby session over Bluetooth Classic/RFCOMM. For large payloads it follows the receiver's Wi-Fi LAN upgrade negotiation, retaining the encryption keys and sequence numbers. Small payloads below Nearby's 1 MB threshold can remain on Bluetooth. Verified with end-to-end file transfers with active VPNs enabled; this is not a guarantee for other network policies.

This implementation supports encoded Nearby Bluetooth names and devices exposing Nearby's SDP service, as well as BLE 0xFEF3 advertisement resolution. Wi-Fi LAN handover still requires direct local TCP reachability.

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

## Nemo right-click sharing

From the extracted native bundle, run `./install-nemo-action` as your normal user.
Select local files in Nemo, right-click and choose **Send with Packet Lab**.
The action supports multiple files, starts Packet Lab if necessary and opens its
recipient picker. It does not send automatically or show for folders.

The action is installed in your XDG data directory under
`nemo/actions/packet-lab.nemo_action`. Keep the bundle in place; run the installer
again after moving it. Remove the action with `./install-nemo-action --uninstall`.
No Nemo Python extension or administrator authorization is required.

## Tray icon

Enable the tray icon in Preferences. Left-click opens Packet Lab; right-click
offers **Open** and **Exit**. A StatusNotifier-compatible desktop panel is required.
The native bundle supplies its symbolic icon and publishes its icon directory
to the panel, so it does not require a system-wide icon installation.

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

The optional cross-process drag regression additionally needs GTK3 PyGObject,
Xvfb and libXtst. After generating the resources above, run it in an isolated bus
and virtual display (it does not connect to Bluetooth or send any files):

```sh
GDK_BACKEND=x11 GIO_USE_VFS=local GSETTINGS_BACKEND=memory \
  GSETTINGS_SCHEMA_DIR="$PWD/_build/data" GTK_A11Y=none \
  DBUS_SYSTEM_BUS_ADDRESS=unix:path=/tmp/packet-lab-no-system-bus \
  xvfb-run -a dbus-run-session --config-file="$PWD/scripts/test-ui-bus.conf" -- \
  cargo test --locked --bin packet-lab window::tests::file_drops -- --ignored --nocapture
```

The portable folder does not install desktop or D-Bus activation entries. Portal host registration can warn that the app is not installed. Auto-start, background permissions and file-manager integration are not validated for this bundle; launch it directly and keep it open for testing.

## Receiving behind a firewall

The default receiver uses a dynamically allocated TCP port. Discovery may work while incoming file transfers time out if the firewall only allows a fixed port. In Preferences, enable the static port and choose a TCP port already permitted on the local network. For example, an existing Packet setup may permit 9300. Only one app can listen on the same port: use different permitted ports when running Packet and Packet Lab together. Keep the VPN enabled; changing this application setting does not change VPN or firewall rules.

## Status

This is an alpha experiment. Compatibility with every school network, Wi-Fi Direct and mid-transfer resume are not promised. A manual address can help when multicast discovery is unavailable; it cannot fix network isolation that also blocks TCP.

Outgoing transfers remain in progress after the final payload chunk until the receiver closes the transfer connection. Receiver errors and a 30-second finalization timeout fail the transfer. This follows the receiver-driven completion ordering in Google Nearby's `OutgoingShareSession::DelayComplete`. File introductions include distinct nonzero attachment IDs for Google's receiver.

## Acknowledgements & Citations

Packet Lab builds upon and owes gratitude to the following open-source projects:

- **[Packet](https://github.com/nozwock/packet)** by [nozwock](https://github.com/nozwock) — The foundational GTK4/Libadwaita Linux Quick Share client.
- **[rquickshare](https://github.com/Martichou/rquickshare)** by [Martichou](https://github.com/Martichou) & [nozwock](https://github.com/nozwock) — The core protocol engine for Quick Share on Linux.
- **[Google Nearby Connections](https://github.com/google/nearby)** — Google's official open-source repository defining the Nearby Connections and Nearby Sharing specifications.
- **[NearDrop](https://github.com/grishka/NearDrop)** by [grishka](https://github.com/grishka) — Pioneering open-source reverse-engineering and implementation of Nearby Share for non-Android platforms.

Source attribution and licenses are in [UPSTREAM.md](UPSTREAM.md) and [LICENSE](LICENSE).
