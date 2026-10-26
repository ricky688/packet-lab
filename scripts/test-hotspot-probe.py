#!/usr/bin/env python3
"""Run against an idle, already running Packet Lab with plabap0 prepared."""
import os
from pathlib import Path
import socket
import time

interface = Path('/sys/class/net/plabap0/ifindex')
assert interface.exists(), 'Prepare the AP before this test; do not run during a transfer'
before = interface.read_text()
log = Path(os.environ.get('XDG_CACHE_HOME', str(Path.home() / '.cache'))) / 'io.github.ricky688.PacketLab/packet-lab.log'
offset = log.stat().st_size
with socket.create_connection(('127.0.0.1', 9300), timeout=2) as probe:
    peer = '127.0.0.1:' + str(probe.getsockname()[1])
deadline = time.monotonic() + 5
while True:
    with log.open() as stream:
        stream.seek(offset)
        observed = stream.read()
    if 'Incoming session ' + peer + ':' in observed:
        break
    assert time.monotonic() < deadline, 'Application did not report the expected probe disconnect'
    time.sleep(.1)
# Allow the GTK receiver and asynchronous network cleanup to process that event.
time.sleep(3)
assert interface.exists() and interface.read_text() == before, 'Probe disconnect tore down the prepared AP'
print('Unintroduced socket disconnect preserves the prepared AP: OK')
