#!/bin/sh
set -eu
prefix=/work/runtime
export PATH="$prefix/bin:/opt/build-tools/bin:$PATH"
export PKG_CONFIG_PATH="$prefix/lib/pkgconfig:$prefix/share/pkgconfig"
export LD_LIBRARY_PATH="$prefix/lib"
mkdir -p /work/sources "$prefix"
python3 - <<'PY'
import hashlib, json, urllib.request
from pathlib import Path
for name, source in json.loads(Path('/source/scripts/appimage-sources.json').read_text()).items():
    target = Path('/work/downloads') / name
    if not target.exists():
        with urllib.request.urlopen(source['url'], timeout=120) as response:
            target.write_bytes(response.read())
    assert hashlib.sha256(target.read_bytes()).hexdigest() == source['sha256'], name
PY

build() {
    # shortcut: version stamps cache builds; clear runtime/sources after changing compiler options.
    name=$1
    shift
    if [ -f "$prefix/.$name-installed" ]; then return; fi
    if [ ! -d "/work/sources/$name" ]; then
        tar -C /work/sources -xf "/work/downloads/$name.tar.xz"
    fi
    if [ -f "/work/sources/$name/_build/build.ninja" ]; then
        meson setup --reconfigure "/work/sources/$name/_build" "/work/sources/$name" "$@"
    else
        meson setup "/work/sources/$name/_build" "/work/sources/$name" \
            --prefix="$prefix" --libdir=lib --buildtype=release --wrap-mode=nofallback \
            -Ddefault_library=shared "$@"
    fi
    ninja -C "/work/sources/$name/_build" -j 4
    ninja -C "/work/sources/$name/_build" install
    touch "$prefix/.$name-installed"
}

build glib-2.84.4 -Dtests=false -Dintrospection=disabled -Ddocumentation=false -Dman-pages=disabled
build harfbuzz-8.4.0 -Dtests=disabled -Dintrospection=disabled -Ddocs=disabled -Dutilities=disabled -Dcairo=disabled -Dicu=disabled
build cairo-1.18.4 -Dtests=disabled -Dgtk_doc=false
build pango-1.56.4 -Dintrospection=disabled -Dbuild-testsuite=false -Dbuild-examples=false
build wayland-1.24.0 -Dtests=false -Ddocumentation=false
build wayland-protocols-1.44 -Dtests=false
build gtk-4.22.0 -Dintrospection=disabled -Ddocumentation=false -Dbuild-tests=false \
    -Dbuild-testsuite=false -Dbuild-examples=false -Dbuild-demos=false \
    -Dvulkan=disabled -Dmedia-gstreamer=disabled -Dprint-cups=disabled
build libadwaita-1.9.0 -Dintrospection=disabled -Dvapi=false -Dtests=false -Dexamples=false -Ddocumentation=false
printf 'Portable runtime ready: GTK %s / libadwaita %s / %s\n' \
    "$(pkg-config --modversion gtk4)" "$(pkg-config --modversion libadwaita-1)" "$(getconf GNU_LIBC_VERSION)"
