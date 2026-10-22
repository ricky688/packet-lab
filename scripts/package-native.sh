#!/bin/sh
set -eu
root=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
cd "$root"
version=$(python -c 'import tomllib; print(tomllib.load(open("Cargo.toml", "rb"))["package"]["version"])')
architecture=$(uname -m)
target_dir=${CARGO_TARGET_DIR:-"$root/target"}
if [ -f _build/build.ninja ]; then
    meson setup --reconfigure _build -Dprofile=default --prefix=/usr
else
    meson setup _build -Dprofile=default --prefix=/usr
fi
ninja -C _build data/resources/resources.gresource src/ui-resources.gresource
glib-compile-schemas _build/data
CARGO_PROFILE_RELEASE_LTO=false cargo build --locked --release --target-dir "$target_dir" -j 2
bundle="$root/dist/packet-lab-$version-linux-$architecture"
mkdir -p "$bundle/bin" "$bundle/share/packet-lab" "$bundle/share/glib-2.0/schemas" \
    "$bundle/share/icons/hicolor/scalable/apps" "$bundle/licenses"
install -m 755 "$target_dir/release/packet-lab" "$bundle/bin/packet-lab"
strip --strip-unneeded "$bundle/bin/packet-lab"
install -m 755 scripts/bundle-launcher.sh "$bundle/packet-lab"
install -m 755 scripts/packet-lab-send "$bundle/packet-lab-send"
install -m 644 _build/data/resources/resources.gresource _build/src/ui-resources.gresource "$bundle/share/packet-lab/"
install -m 644 _build/data/*.gschema.xml _build/data/gschemas.compiled "$bundle/share/glib-2.0/schemas/"
install -m 644 data/icons/io.github.ricky688.PacketLab.svg "$bundle/share/icons/hicolor/scalable/apps/"
install -m 644 LICENSE "$bundle/licenses/Packet-GPL-3.0.txt"
install -m 644 subprojects/rqs_lib/LICENSE "$bundle/licenses/rquickshare-GPL-3.0.txt"
install -m 644 README.md UPSTREAM.md CHANGELOG.md "$bundle/"
install -m 644 docs/LOCAL-VALIDATION.md "$bundle/LOCAL-VALIDATION.md"
{
    printf 'Packet Lab %s\nArchitecture: %s\n' "$version" "$architecture"
    printf '\nBuild environment:\n'
    getconf GNU_LIBC_VERSION
    printf 'GTK4: '; pkg-config --modversion gtk4
    printf 'libadwaita: '; pkg-config --modversion libadwaita-1
    printf 'GLib: '; pkg-config --modversion glib-2.0
    printf '\nNative test bundle; compatible system libraries are required.\n'
    printf 'Not a Flatpak or universal AppImage.\n'
} > "$bundle/BUILD-INFO.txt"
python - "$bundle" <<'PY'
from pathlib import Path
import subprocess, sys
root = Path(sys.argv[1])
assert (root / "packet-lab").is_file()
assert (root / "share/glib-2.0/schemas/gschemas.compiled").is_file()
assert (root / "bin/packet-lab").read_bytes().startswith(b"\x7fELF")
result = subprocess.run([str(root / "packet-lab"), "--help"], capture_output=True, text=True, timeout=15)
assert result.returncode == 0, result.stderr
assert "Application Options" in result.stdout
PY
name=$(basename "$bundle")
tar -C "$root/dist" -czf "$bundle.tar.gz" "$name"
(cd "$root/dist"; sha256sum "$name.tar.gz" > "$name.tar.gz.sha256")
printf '\nBundle: %s\nArchive: %s.tar.gz\n' "$bundle" "$bundle"
