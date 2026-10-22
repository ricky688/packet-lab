#!/bin/sh
set -eu
root=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
export MESON_DEVENV=1
export GSETTINGS_SCHEMA_DIR="$root/share/glib-2.0/schemas"
export RESOURCE_FILE="$root/share/packet-lab/resources.gresource"
export UI_RESOURCE_FILE="$root/share/packet-lab/ui-resources.gresource"
export XDG_DATA_DIRS="$root/share:${XDG_DATA_DIRS:-/usr/local/share:/usr/share}"
exec "$root/bin/packet-lab" "$@"
