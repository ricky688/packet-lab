#!/usr/bin/env python3
"""Temporary BlueZ reverse-GATT diagnostic; stop with a newline or SIGTERM to restore."""
import os
from pathlib import Path
import signal
import subprocess
import tempfile


def diagnostic_config(source):
    assert source.count('#Client = true') == 1, 'Unexpected GATT configuration; refusing to modify'
    return source.replace('#Client = true', 'Client = false', 1)


if __name__ == '__main__':
    import sys
    if '--self-test' in sys.argv:
        assert diagnostic_config('[GATT]\n#Client = true\n') == '[GATT]\nClient = false\n'
        try:
            diagnostic_config('[GATT]\nClient = true\n')
        except AssertionError:
            pass
        else:
            raise AssertionError('Must reject unexpected configuration')
        print('Temporary GATT configuration validation: OK')
        sys.exit(0)
    assert os.geteuid() == 0, 'System authorization required'
    assert not subprocess.check_output(['bluetoothctl', 'devices', 'Connected'], text=True).strip(), (
        'A Bluetooth device is connected; stop before changing service'
    )
    override = Path('/run/systemd/system/bluetooth.service.d/zz-packet-lab-gatt-probe.conf')
    config = diagnostic_config(Path('/etc/bluetooth/main.conf').read_text())

    def run(*args):
        subprocess.run(args, check=True)

    def stop(*_):
        raise SystemExit()

    signal.signal(signal.SIGTERM, stop)
    signal.signal(signal.SIGHUP, stop)
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
            run('systemctl', 'daemon-reload')
            run('systemctl', 'restart', 'bluetooth')
            assert str(config_path) in subprocess.check_output(
                ['systemctl', 'show', 'bluetooth.service', '-p', 'ExecStart'], text=True
            ), 'Diagnostic config was not selected'
            print('Temporary reverse-GATT diagnostic ready; press Enter to restore', flush=True)
            input()
        finally:
            override.unlink(missing_ok=True)
            run('systemctl', 'daemon-reload')
            run('systemctl', 'restart', 'bluetooth')
            print('Original Bluetooth service configuration restored', flush=True)
