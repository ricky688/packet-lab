#!/usr/bin/env python3
"""Test BlueZ inbound reverse discovery; stop with Ctrl+C or SIGTERM to restore."""
import argparse
import os
from pathlib import Path
import re
import signal
import subprocess
import tempfile


def diagnostic_config(source):
    general = re.search(r'^\[General\][ \t]*\n(.*?)(?=^\[|\Z)', source, re.MULTILINE | re.DOTALL)
    if not general:
        raise ValueError('Missing [General] section in BlueZ configuration')
    start, end = general.span(1)
    section, changed = re.subn(r'^[ \t]*#?[ \t]*ReverseServiceDiscovery[ \t]*=[ \t]*(?:true|false)[ \t]*$',
                               'ReverseServiceDiscovery = false', source[start:end],
                               flags=re.MULTILINE)
    if changed != 1:
        raise ValueError('Expected one ReverseServiceDiscovery setting in [General]')
    return source[:start] + section + source[end:]


if __name__ == '__main__':
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--self-test', action='store_true')
    parser.add_argument('--allow-connected', action='store_true')
    args = parser.parse_args()
    if args.self_test:
        config = '[General]\n#ReverseServiceDiscovery = true\n[GATT]\n#Client = true\n'
        updated = '[General]\nReverseServiceDiscovery = false\n[GATT]\n#Client = true\n'
        assert diagnostic_config(config) == updated
        assert diagnostic_config(updated) == updated
        try:
            diagnostic_config('[GATT]\n#Client = true\n')
        except ValueError:
            pass
        else:
            raise AssertionError('Must reject unexpected configuration')
        print('Temporary reverse discovery configuration validation: OK')
        raise SystemExit
    assert os.geteuid() == 0, 'System authorization required'
    if not args.allow_connected and subprocess.check_output(
            ['/usr/bin/bluetoothctl', 'devices', 'Connected'], text=True).strip():
        raise RuntimeError('A Bluetooth device is connected; stop before changing service')
    override = Path('/run/systemd/system/bluetooth.service.d/zz-packet-lab-gatt-probe.conf')
    config = diagnostic_config(Path('/etc/bluetooth/main.conf').read_text())

    def systemctl(*args):
        subprocess.run(['/usr/bin/systemctl', *args], check=True)

    def stop(*_):
        raise SystemExit()

    signal.signal(signal.SIGTERM, stop)
    signal.signal(signal.SIGHUP, stop)
    signal.signal(signal.SIGALRM, stop)
    override.parent.mkdir(parents=True, exist_ok=True)
    with tempfile.TemporaryDirectory(prefix='packet-lab-bluez-', dir='/run') as directory:
        config_path = Path(directory) / 'main.conf'
        config_path.write_text(config)
        config_path.chmod(0o600)
        fd = os.open(override, os.O_WRONLY | os.O_CREAT | os.O_EXCL | os.O_NOFOLLOW, 0o600)
        try:
            with os.fdopen(fd, 'w') as out:
                out.write(
                    '[Service]\nExecStart=\nExecStart=/usr/lib/bluetooth/bluetoothd -E -f '
                    + str(config_path)
                    + '\n'
                )
            systemctl('daemon-reload')
            systemctl('restart', 'bluetooth')
            assert str(config_path) in subprocess.check_output(
                ['/usr/bin/systemctl', 'show', 'bluetooth.service', '-p', 'ExecStart'], text=True
            ), 'Diagnostic config was not selected'
            print('Temporary reverse discovery diagnostic ready; press Ctrl+C to restore '
                  'or wait for automatic restoration in 3 minutes', flush=True)
            signal.alarm(180)
            signal.pause()
        finally:
            signal.alarm(0)
            override.unlink(missing_ok=True)
            systemctl('daemon-reload')
            systemctl('restart', 'bluetooth')
            print('Original Bluetooth service configuration restored', flush=True)
