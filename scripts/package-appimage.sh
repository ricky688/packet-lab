#!/bin/sh
set -eu
root=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
cd "$root"
[ "$(uname -m)" = x86_64 ] || { echo 'This recipe builds x86_64 only' >&2; exit 1; }
version=$(python3 -c 'import tomllib; print(tomllib.load(open("Cargo.toml", "rb"))["package"]["version"])')
[ -x "dist/packet-lab-$version-linux-x86_64/bin/packet-lab" ] || {
    echo 'Build the native bundle with scripts/package-native.sh first' >&2; exit 1;
}
cache="$root/.local-validation/appimage"
mkdir -p "$cache/downloads"
python3 - <<'PY'
from pathlib import Path
import hashlib, json, subprocess, tarfile, urllib.request
cache = Path('.local-validation/appimage')
for manifest in ('appimage-sources.json', 'appimage-tools.json'):
    for name, source in json.loads((Path('scripts') / manifest).read_text()).items():
        path = cache / 'downloads' / name
        if not path.exists():
            with urllib.request.urlopen(source['url'], timeout=120) as response:
                path.write_bytes(response.read())
        assert hashlib.sha256(path.read_bytes()).hexdigest() == source['sha256'], f'Checksum mismatch: {name}'
        if manifest == 'appimage-tools.json': path.chmod(0o755)
files = subprocess.check_output(['git', 'ls-files', '-z', '--cached', '--others', '--exclude-standard']).decode().split('\0')
(cache / 'source-revision.txt').write_text(subprocess.check_output(['git', 'rev-parse', 'HEAD'], text=True).strip() + '\n')
with tarfile.open(cache / 'packet-lab-source.tar.gz', 'w:gz') as archive:
    for name in sorted(set(files) - {''}):
        archive.add(name, arcname='packet-lab/' + name, recursive=False)
PY
docker build --network=host -t packet-lab-appimage-builder -f scripts/appimage.Dockerfile scripts
docker run --rm --network=none \
    --mount "type=bind,src=$root,dst=/source,readonly" \
    --mount "type=bind,src=$cache,dst=/work" \
    packet-lab-appimage-builder sh -ec '
        sh /source/scripts/build-appimage-runtime.sh
        python3 /source/scripts/assemble-appimage.py
        ARCH=x86_64 APPIMAGE_EXTRACT_AND_RUN=1 /work/downloads/appimagetool-x86_64.AppImage \
            --runtime-file /work/downloads/runtime-x86_64 --comp zstd \
            --mksquashfs-opt=-processors --mksquashfs-opt=2 \
            /work/PacketLab.AppDir /work/Packet_Lab.AppImage
    '
output="Packet_Lab-$version-x86_64.AppImage"
install -m 755 "$cache/Packet_Lab.AppImage" "dist/$output"
(cd dist; sha256sum "$output" > "$output.sha256")
printf '\nAppImage: %s/dist/%s\n' "$root" "$output"
