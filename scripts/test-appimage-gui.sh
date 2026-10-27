#!/bin/sh
# Run inside the minimal Xvfb/D-Bus container with /test/gapplication and /test/lib mounted.
set -eu
export DBUS_SYSTEM_BUS_ADDRESS="$DBUS_SESSION_BUS_ADDRESS"
export GTK_A11Y=none GSETTINGS_BACKEND=memory
log=/tmp/packet-lab-appimage-gui.log
"$1" --appimage-extract-and-run > "$log" 2>&1 &
pid=$!
trap 'kill "$pid" 2>/dev/null || true' EXIT
ready=false
for attempt in $(seq 1 100); do
    kill -0 "$pid" || { cat "$log"; exit 1; }
    if dbus-send --session --print-reply --dest=org.freedesktop.DBus \
        /org/freedesktop/DBus org.freedesktop.DBus.NameHasOwner \
        string:io.github.ricky688.PacketLab | grep -q 'boolean true'; then
        ready=true
        break
    fi
    sleep 0.1
done
[ "$ready" = true ] || { cat "$log"; exit 1; }
for attempt in $(seq 1 100); do
    grep -q 'Starting mDNS discovery task' "$log" && break
    kill -0 "$pid" || { cat "$log"; exit 1; }
    sleep 0.1
done
grep -q 'Starting mDNS discovery task' "$log" || { cat "$log"; exit 1; }
kill -0 "$pid"
if grep -E 'symbol lookup error|error while loading shared libraries|Gtk-CRITICAL|GLib-GObject-CRITICAL|panicked at' "$log"; then
    cat "$log"
    exit 1
fi
LD_LIBRARY_PATH=/test/lib /test/gapplication action io.github.ricky688.PacketLab quit
wait "$pid"
trap - EXIT
echo 'AppImage GTK startup, automatic discovery, D-Bus registration and graceful quit passed without host GTK'
