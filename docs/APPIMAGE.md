# AppImage alpha

The x86_64 AppImage packages GTK 4.22, libadwaita 1.9 and their application
libraries. Its target minimum is glibc 2.39 (Ubuntu 24.04 baseline). This is an
alpha build; the supported systems and actual test results are listed with the
artifact. It is not an ARM build or a promise of compatibility with every Linux
distribution.

## Start and share files

Keep the AppImage in a permanent location, verify the adjacent checksum file,
then allow execution and start it:

```sh
sha256sum -c Packet_Lab-0.1.0-alpha.1-x86_64.AppImage.sha256
chmod +x Packet_Lab-0.1.0-alpha.1-x86_64.AppImage
./Packet_Lab-0.1.0-alpha.1-x86_64.AppImage
```

Enable Bluetooth and Wi-Fi. Opening or activating Packet Lab automatically enables
receive visibility and starts discovery; scanning continues while its main window
is visible. On Android, open Quick Share's receive screen.
Drop files onto Packet Lab's outlined area, choose a recipient and accept on the
receiving device. To receive on Linux, keep Packet Lab visible and accept its
receive prompt. VPNs may stay enabled; their routing/firewall policy can still
block a local link. Bluetooth remains the slower fallback when Wi-Fi upgrades
cannot be established.

Exit an existing Packet Lab process before switching between the native bundle
and AppImage: they share an application ID, so starting one while the other is
running normally presents the existing instance.

BlueZ, NetworkManager, polkit, Wi-Fi/Bluetooth drivers and a working session
D-Bus remain host services. They cannot be replaced by the AppImage's GTK
libraries. The tray requires a StatusNotifier-compatible panel. The AppImage
uses GTK's Cairo renderer by default and persists preferences with GSettings'
keyfile backend, independently of a native bundle's dconf preferences.

**Auto Start** registers the persistent AppImage file with the desktop's XDG
autostart service. **Run in Background** keeps the receiver running when its
window is closed. Received files go to `~/Downloads` unless you select another
folder in Preferences.

If FUSE is unavailable, the runtime can extract and run the same image:

```sh
./Packet_Lab-0.1.0-alpha.1-x86_64.AppImage --appimage-extract-and-run
```

## Nemo integration (optional)

Host Python 3 with PyGObject/Gio is needed for file-manager integration. Install
the per-user action as your normal user:

```sh
./Packet_Lab-0.1.0-alpha.1-x86_64.AppImage --install-nemo-action
```

In Nemo, select local files and choose **Send with Packet Lab**. This opens the
recipient picker without choosing or sending to a device automatically. The
action refers to the AppImage file, not its temporary mount. If you move or
rename the image, run the command again. Remove it with
`--uninstall-nemo-action`.

For a Thunar custom action, use the absolute path to the AppImage followed by
`--send-files %F` (quote the AppImage path, leave `%F` unquoted). Folders and empty
files are not supported.

## Optional fixed network helpers

Private Hotspot hosting and legacy Wi-Fi Direct joining require host Python 3
with PyGObject, iw, iproute2, wpa_supplicant/wpa_cli and dnsmasq; AP DHCP also uses
iptables. Basic LAN transfers do not require installing these helpers. See
[handover requirements](HOTSPOT-HANDOVER.md).

Do not install privileged scripts from a temporary AppImage mount. First extract
the four fixed setup files into a new persistent directory:

```sh
./Packet_Lab-0.1.0-alpha.1-x86_64.AppImage --extract-network-helper ./packet-lab-network-helper
sudo /usr/bin/python3 -I ./packet-lab-network-helper/install-network-helper.py --user "$USER"
```

This explicitly authorizes only the two root-owned AP/P2P helpers for the chosen
account, including background/SSH processes under that account. It does not
grant general Python, shell or sudo access. Quick Share receive confirmation is
retained. Updating the AppImage does not update installed root-owned copies;
extract into a new directory and rerun the installer when helper updates are
needed. Remove the installation with the extracted installer and `--uninstall`.

## Build

Generate the current native bundle first, then run:

```sh
./scripts/package-appimage.sh
python3 scripts/test-appimage.py
```

The recipe uses Docker and the host's existing network route for container-only
package downloads, builds the locked GTK sources on Ubuntu 24.04, verifies source
and tool SHA-256 values, then packages offline. Host packages, Wi-Fi, VPN and
firewall configuration are not changed. The initial build needs several GB of
disk and takes longer than subsequent cached builds. Build data is under the
ignored `.local-validation/appimage/`; output and checksum are in `dist/`.

The assembler refuses libraries requiring glibc newer than 2.39. Source archives,
runtime dependency metadata and license notices are included inside the image
under `usr/sources`, `usr/share/packet-lab/APPIMAGE-BUILD.json` and `usr/licenses`.
Upstream continuous AppImage tools are checksum-locked: if their assets change,
review the new upstream digest before updating `scripts/appimage-tools.json`.

## Verification on 2026-10-27

- Assembled 99 runtime libraries, with a maximum required glibc symbol version of 2.39.
- Command-line startup passed in the unmodified Ubuntu 24.04 base image with no GTK installed, using extract-and-run.
- GUI startup, private D-Bus registration and graceful exit passed under Xvfb in a minimal Ubuntu 24.04 container without host GTK. The smoke image recipe and test are `scripts/appimage-smoke.Dockerfile` and `scripts/test-appimage-gui.sh`.
- Normal FUSE startup passed on the build host. Actual AppImage Nemo installation/removal and network-helper export passed in temporary directories without changing the installed action or host network permissions.
- Five integration checks, seven P2P-helper checks and AP/Nemo self-checks passed. Exported setup files retained the fixed-helper authorization boundary.

The AppImage has not yet been verified with real phone transfers, Wi-Fi Direct,
Nemo mouse/menu interaction or other distributions. Earlier native-bundle phone
tests do not establish those AppImage results. The isolated container tests have
no Bluetooth/Wi-Fi hardware or host network services.

## First use with another person's phone

Bluetooth pairing is not required for discovery. Keep the phone unlocked, enable
Bluetooth and Wi-Fi, and open Quick Share's receive screen with visibility set to
Everyone (10 minutes), rather than contacts-only. Packet Lab must be open.
The app retries brief BlueZ busy errors automatically; it does not bypass pairing
authorization or Quick Share receive confirmation. If discovery still fails,
record the Linux distribution, Bluetooth adapter and Packet Lab log from that PC;
a successful test on another PC does not verify its Bluetooth driver.

## Handover latency

Direct-host preparation is shared within each incoming transfer, so preparation
can overlap receive confirmation and the LAN attempt. A validated LAN rejection
or timeout is remembered for that peer for 30 seconds within the current sharing
service; repeated transfers can try the direct host immediately. Another peer,
expiry or a network-service restart restores the normal LAN attempt. Existing
connection deadlines and receive consent are retained. This optimization does not
guarantee a particular Wi-Fi Direct setup time on every adapter or VPN.
