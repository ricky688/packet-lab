#!/usr/bin/python3 -I
"""Own one transient AP; keep the station and VPN under NetworkManager."""
import fcntl
import os
import re
import secrets
import select
import subprocess
import sys
import tempfile
import time
from contextlib import contextmanager
from gi.repository import Gio, GLib

NAME = 'plabap0'
NM = 'org.freedesktop.NetworkManager'
IDLE_LEASE_SECONDS = 300


def ap_traffic_bytes(iface=NAME):
    total = 0
    for counter in ('rx_bytes', 'tx_bytes'):
        try:
            with open(f'/sys/class/net/{iface}/statistics/{counter}') as stream:
                total += int(stream.read().strip())
        except OSError:
            pass
    return total


def extend_lease_on_activity(now, deadline, prev_bytes, curr_bytes, idle_seconds=IDLE_LEASE_SECONDS):
    if curr_bytes > prev_bytes:
        return max(deadline, now + idle_seconds), curr_bytes
    return deadline, curr_bytes


@contextmanager
def permit_ap_dhcp():
    # UFW drops DHCP-server requests even when the AP has associated successfully.
    rule = ['-i', NAME, '-p', 'udp', '--sport', '68', '--dport', '67',
            '-m', 'comment', '--comment', 'packet-lab-dhcp-' + secrets.token_hex(8),
            '-j', 'ACCEPT']
    subprocess.run(['/usr/bin/iptables', '-w', '5', '-I', 'INPUT', '1', *rule], check=True)
    try:
        yield
    finally:
        subprocess.run(['/usr/bin/iptables', '-w', '5', '-D', 'INPUT', *rule], check=True)


def valid_parent(name):
    return bool(re.fullmatch(r'[A-Za-z0-9_.-]{1,15}', name)) and name not in ('.', '..', NAME)


def channel(frequency):
    if 2412 <= frequency <= 2472 and (frequency - 2407) % 5 == 0:
        return (frequency - 2407) // 5
    if 5180 <= frequency <= 5885 and (frequency - 5000) % 5 == 0:
        return (frequency - 5000) // 5
    raise ValueError('Unsupported AP frequency')


def run(parent, frequency):
    if os.geteuid() != 0 or not valid_parent(parent):
        raise RuntimeError('Requires root and a valid Wi-Fi parent')
    channel(frequency)
    lock = os.open('/run/lock/packet-lab-ap.lock', os.O_CREAT | os.O_RDWR | os.O_NOFOLLOW, 0o600)
    if os.fstat(lock).st_uid != 0:
        raise RuntimeError('AP lock is not root-owned')
    fcntl.flock(lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
    if os.path.exists('/sys/class/net/' + NAME):
        raise RuntimeError('AP name already exists; refusing to modify it')
    bus = Gio.bus_get_sync(Gio.BusType.SYSTEM, None)

    def call(path, iface, method, args=None):
        return bus.call_sync(NM, path, iface, method, args, None,
                             Gio.DBusCallFlags.NONE, 5000, None).unpack()

    def prop(path, iface, key):
        return call(path, 'org.freedesktop.DBus.Properties', 'Get',
                    GLib.Variant('(ss)', (iface, key)))[0]

    root = '/org/freedesktop/NetworkManager'
    station = call(root, NM, 'GetDeviceByIpIface', GLib.Variant('(s)', (parent,)))[0]
    original = prop(station, NM + '.Device', 'ActiveConnection')
    state = prop(station, NM + '.Device', 'State')
    station_connected = (state == 100)
    if station_connected:
        ap = prop(station, NM + '.Device.Wireless', 'ActiveAccessPoint')
        if prop(ap, NM + '.AccessPoint', 'Frequency') != frequency:
            raise RuntimeError('Parent frequency changed')
    elif state < 30:
        raise RuntimeError(f'Parent Wi-Fi device is unavailable (state={state})')
    children = []
    temp_dir = None
    created = False
    try:
        if not station_connected:
            subprocess.run(['/usr/bin/ip', 'link', 'set', parent, 'up'], check=False)
        subprocess.run(['/usr/bin/iw', 'dev', parent, 'interface', 'add', NAME,
                        'type', '__ap'], check=True)
        created = True
        deadline = time.monotonic() + 5
        device = None
        while time.monotonic() < deadline:
            try:
                device = call(root, NM, 'GetDeviceByIpIface', GLib.Variant('(s)', (NAME,)))[0]
                break
            except GLib.Error:
                time.sleep(.1)
        if device is None:
            raise RuntimeError('NM did not discover the AP interface')
        call(device, 'org.freedesktop.DBus.Properties', 'Set',
             GLib.Variant('(ssv)', (NM + '.Device', 'Managed', GLib.Variant('b', False))))
        subprocess.run(['/usr/bin/ip', 'link', 'set', NAME, 'down'], check=True)
        subprocess.run(['/usr/bin/iw', 'dev', NAME, 'set', 'type', '__ap'], check=True)
        temp_dir = tempfile.TemporaryDirectory(prefix='packet-lab-ap-', dir='/run')
        temp = temp_dir.name
        ssid = 'PacketLab-' + secrets.token_hex(3)
        password = secrets.token_hex(16)
        config = temp + '/ap.conf'
        fd = os.open(config, os.O_WRONLY | os.O_CREAT | os.O_EXCL, 0o600)
        with os.fdopen(fd, 'w') as stream:
            stream.write('ctrl_interface=' + temp + '/ctrl\np2p_disabled=1\nap_scan=2\nnetwork={\n'
                         'ssid="' + ssid + '"\nmode=2\nfrequency=' + str(frequency) + '\n'
                         'key_mgmt=WPA-PSK\nproto=RSN\npairwise=CCMP\n'
                         'psk="' + password + '"\n}\n')
        wpa = subprocess.Popen(['/usr/bin/wpa_supplicant', '-Dnl80211', '-i', NAME, '-c', config],
                               stdout=sys.stderr, stderr=sys.stderr)
        children.append(wpa)
        deadline = time.monotonic() + 10
        while True:
            if wpa.poll() is not None:
                raise RuntimeError('Private AP supplicant exited')
            status = subprocess.run(['/usr/bin/wpa_cli', '-p', temp + '/ctrl', '-i', NAME, 'status'],
                                    capture_output=True, text=True, timeout=2).stdout
            if 'wpa_state=COMPLETED' in status:
                if 'freq=' + str(frequency) not in status.splitlines():
                    raise RuntimeError('AP frequency does not match the station')
                break
            if time.monotonic() >= deadline:
                raise RuntimeError('AP readiness timed out')
            time.sleep(.1)
        if station_connected and (prop(station, NM + '.Device', 'State') != 100 or prop(station, NM + '.Device', 'ActiveConnection') != original):
            raise RuntimeError('AP affected the original Wi-Fi connection')
        subprocess.run(['/usr/bin/ip', 'address', 'add', '192.168.49.1/24', 'dev', NAME], check=True)
        with permit_ap_dhcp():
            dhcp = subprocess.Popen(['/usr/bin/dnsmasq', '--keep-in-foreground', '--conf-file=/dev/null',
                '--port=0', '--bind-interfaces', '--interface=' + NAME,
                '--dhcp-range=192.168.49.2,192.168.49.254,255.255.255.0,15m',
                '--dhcp-option=3', '--dhcp-option=6', '--dhcp-authoritative',
                '--dhcp-leasefile=' + temp + '/leases', '--pid-file='],
                stdout=sys.stderr, stderr=sys.stderr)
            children.append(dhcp)
            time.sleep(.2)
            if dhcp.poll() is not None:
                raise RuntimeError('AP DHCP server exited')
            print('\t'.join([NAME, ssid, password, str(frequency)]), flush=True)
            deadline = time.monotonic() + IDLE_LEASE_SECONDS
            last_bytes = ap_traffic_bytes()
            while time.monotonic() < deadline and all(child.poll() is None for child in children):
                readable, _, _ = select.select([sys.stdin], [], [], 1)
                if readable:
                    chunk = sys.stdin.read(1)
                    if not chunk:
                        break
                    deadline = max(deadline, time.monotonic() + IDLE_LEASE_SECONDS)
                deadline, last_bytes = extend_lease_on_activity(
                    time.monotonic(), deadline, last_bytes, ap_traffic_bytes()
                )
    finally:
        for child in reversed(children):
            if child.poll() is None:
                child.terminate()
                try:
                    child.wait(timeout=5)
                except subprocess.TimeoutExpired:
                    child.kill()
                    child.wait()
        if temp_dir is not None:
            temp_dir.cleanup()
        if created:
            subprocess.run(['/usr/bin/iw', 'dev', NAME, 'del'], check=True)


if __name__ == '__main__':
    if sys.argv[1:] == ['--self-test']:
        assert valid_parent('wlo1') and not any(valid_parent(x) for x in ('../wlo1', '.', '..', NAME, 'x' * 16))
        assert channel(5745) == 149 and channel(2462) == 11
        new_dl, new_bytes = extend_lease_on_activity(100.0, 200.0, 500, 1500, 300)
        assert new_dl == 400.0 and new_bytes == 1500
        same_dl, same_bytes = extend_lease_on_activity(100.0, 200.0, 1500, 1500, 300)
        assert same_dl == 200.0 and same_bytes == 1500
        print('AP helper validation: OK')
    else:
        try:
            if len(sys.argv) != 3:
                raise RuntimeError('Usage: wifi-ap-helper.py PARENT FREQUENCY')
            import signal
            signal.signal(signal.SIGTERM, lambda *_: sys.exit(0))
            run(sys.argv[1], int(sys.argv[2]))
        except Exception as exc:
            print('Packet Lab AP helper: ' + str(exc), file=sys.stderr)
            sys.exit(1)
