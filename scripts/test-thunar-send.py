#!/usr/bin/env python3
"""Run with dbus-run-session -- python scripts/test-thunar-send.py."""
from pathlib import Path
import shutil
import subprocess
import tempfile
import time

from gi.repository import Gio, GLib


with tempfile.TemporaryDirectory(prefix="packet-lab-thunar-") as directory:
    root = Path(directory)
    helper = root / "packet-lab-send"
    shutil.copy2(Path(__file__).with_name("packet-lab-send"), helper)
    launcher = root / "packet-lab"
    launcher.write_text("#!/bin/sh\nexit 0\n")
    launcher.chmod(0o755)
    files = [root / "two words.txt", root / "繁體 'quoted' $(touch INJECTED).txt"]
    for file in files:
        file.write_text("synthetic test\n")
    bus = Gio.bus_get_sync(Gio.BusType.SESSION, None)
    bus.call_sync("org.freedesktop.DBus", "/org/freedesktop/DBus",
                  "org.freedesktop.DBus", "RequestName",
                  GLib.Variant("(su)", ("io.github.ricky688.PacketLab", 0)),
                  None, Gio.DBusCallFlags.NONE, 1000, None)
    received = []
    group = Gio.SimpleActionGroup()
    action = Gio.SimpleAction.new("send-files", GLib.VariantType.new("as"))
    action.connect("activate", lambda _, value: received.append(value.unpack()))
    group.add_action(action)
    export = bus.export_action_group("/io/github/ricky688/PacketLab/Share", group)
    process = subprocess.Popen([str(helper), *map(str, files)])
    deadline = time.monotonic() + 12
    context = GLib.MainContext.default()
    while (process.poll() is None or not received) and time.monotonic() < deadline:
        while context.pending():
            context.iteration(False)
        time.sleep(0.01)
    if process.poll() is None:
        process.kill()
    assert process.wait() == 0
    assert received == [[str(file) for file in files]], received
    assert not (root / "INJECTED").exists()
    for args in [[], [str(root)], [str(root / "missing")]]:
        result = subprocess.run([str(helper), *args], capture_output=True, timeout=3)
        assert result.returncode == 1
    bus.unexport_action_group(export)
    print("PASS: multi-file D-Bus handoff, spaces/quotes/Unicode, invalid input rejection")
