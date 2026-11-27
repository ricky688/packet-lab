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


def system_tool(name):
    # Ignore caller PATH; Debian and Arch place administrative tools differently.
    for directory in ('/usr/bin', '/usr/sbin'):
        path = directory + '/' + name
        if os.path.isfile(path) and os.access(path, os.X_OK):
            return path
    raise FileNotFoundError('Required system tool is missing: ' + name)


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
def permit_ap_dhcp(iface=NAME):
    # UFW drops DHCP-server requests even when the AP has associated successfully.
    rule = ['-i', iface, '-p', 'udp', '--sport', '68', '--dport', '67',
            '-m', 'comment', '--comment', 'packet-lab-dhcp-' + secrets.token_hex(8),
            '-j', 'ACCEPT']
    subprocess.run([system_tool('iptables'), '-w', '5', '-I', 'INPUT', '1', *rule], check=True)
    try:
        yield
    finally:
        subprocess.run([system_tool('iptables'), '-w', '5', '-D', 'INPUT', *rule], check=True)


def valid_parent(name):
    return bool(re.fullmatch(r'[A-Za-z0-9_.-]{1,15}', name)) and name not in ('.', '..', NAME)


def channel(frequency):
    if 2412 <= frequency <= 2472 and (frequency - 2407) % 5 == 0:
        return (frequency - 2407) // 5
    if 5180 <= frequency <= 5885 and (frequency - 5000) % 5 == 0:
        return (frequency - 5000) // 5
    raise ValueError('Unsupported AP frequency')


class ActiveGoError(RuntimeError):
    pass


def supplicant_config(ssid, password, frequency, go):
    return ('ctrl_interface={ctrl}\n' + ('p2p_no_group_iface=1\np2p_disabled=0\n' if go else 'p2p_disabled=1\n')
            + 'ap_scan=2\nnetwork={\nssid="' + ssid + '"\nmode=' + ('3' if go else '2')
            + '\nfrequency=' + str(frequency) + '\nkey_mgmt=WPA-PSK\nproto=RSN\npairwise=CCMP\n'
            + 'psk="' + password + '"\n}\n')


def p2p_go_iface_name(parent, suffix):
    suffix_str = str(suffix)
    max_prefix = 15 - 5 - len(suffix_str)
    if max_prefix < 1:
        raise ValueError('P2P interface name would exceed the kernel limit')
    return f'p2p-{parent[:max_prefix]}-{suffix_str}'


def run(parent, frequency, go=False):
    if os.geteuid() != 0 or not valid_parent(parent):
        raise RuntimeError('Requires root and a valid Wi-Fi parent')
    if go:
        frequency = 2412
    channel(frequency)
    iface = p2p_go_iface_name(parent, secrets.randbelow(100)) if go else NAME
    if len(iface) > 15:
        raise ValueError('P2P interface name would exceed the kernel limit')
    ready = False
    lock = os.open('/run/lock/packet-lab-ap.lock', os.O_CREAT | os.O_RDWR | os.O_NOFOLLOW, 0o600)
    if os.fstat(lock).st_uid != 0:
        raise RuntimeError('AP lock is not root-owned')
    fcntl.flock(lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
    if os.path.exists('/sys/class/net/' + iface):
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
        if not go and prop(ap, NM + '.AccessPoint', 'Frequency') != frequency:
            raise RuntimeError('Parent frequency changed')
    elif state < 30:
        raise RuntimeError(f'Parent Wi-Fi device is unavailable (state={state})')
    children = []
    temp_dir = None
    created = False
    try:
        if not station_connected:
            subprocess.run([system_tool('ip'), 'link', 'set', parent, 'up'], check=False)
        subprocess.run([system_tool('iw'), 'dev', parent, 'interface', 'add', iface,
                        'type', '__p2pgo' if go else '__ap'], check=True)
        created = True
        deadline = time.monotonic() + (.5 if go else 5)
        device = None
        while time.monotonic() < deadline:
            try:
                device = call(root, NM, 'GetDeviceByIpIface', GLib.Variant('(s)', (iface,)))[0]
                break
            except GLib.Error:
                time.sleep(.1)
        if device is None and not go:
            raise RuntimeError('NM did not discover the AP interface')
        if device is not None:
            call(device, 'org.freedesktop.DBus.Properties', 'Set',
                 GLib.Variant('(ssv)', (NM + '.Device', 'Managed', GLib.Variant('b', False))))
        subprocess.run([system_tool('ip'), 'link', 'set', iface, 'down'], check=True)
        subprocess.run([system_tool('iw'), 'dev', iface, 'set', 'type', '__p2pgo' if go else '__ap'], check=True)
        if go:
            mac = bytearray(secrets.token_bytes(6))
            mac[0] = (mac[0] & 0xfc) | 2
            subprocess.run([system_tool('ip'), 'link', 'set', 'dev', iface, 'address',
                            ':'.join(f'{byte:02x}' for byte in mac)], check=True)
        temp_dir = tempfile.TemporaryDirectory(prefix='packet-lab-ap-', dir='/run')
        temp = temp_dir.name
        ssid = ('DIRECT-' if go else 'PacketLab-') + secrets.token_hex(3)
        password = secrets.token_hex(16)
        config = temp + '/ap.conf'
        fd = os.open(config, os.O_WRONLY | os.O_CREAT | os.O_EXCL, 0o600)
        with os.fdopen(fd, 'w') as stream:
            stream.write(supplicant_config(ssid, password, frequency, go).replace('{ctrl}', temp + '/ctrl'))
        wpa = subprocess.Popen([system_tool('wpa_supplicant'), '-Dnl80211', '-i', iface, '-c', config],
                               stdout=sys.stderr, stderr=sys.stderr)
        children.append(wpa)
        deadline = time.monotonic() + 10
        while True:
            if wpa.poll() is not None:
                raise RuntimeError('Private AP supplicant exited')
            status = subprocess.run([system_tool('wpa_cli'), '-p', temp + '/ctrl', '-i', iface, 'status'],
                                    capture_output=True, text=True, timeout=2).stdout
            if 'wpa_state=COMPLETED' in status:
                if 'freq=' + str(frequency) not in status.splitlines() or ('ssid=' + ssid) not in status.splitlines():
                    raise RuntimeError('AP frequency does not match the station')
                break
            if time.monotonic() >= deadline:
                raise RuntimeError('AP readiness timed out')
            time.sleep(.1)
        if station_connected and (prop(station, NM + '.Device', 'State') != 100 or prop(station, NM + '.Device', 'ActiveConnection') != original):
            raise RuntimeError('AP affected the original Wi-Fi connection')
        if go:
            info = subprocess.run([system_tool('iw'), 'dev', iface, 'info'], capture_output=True, text=True, check=True).stdout
            if 'type P2P-GO' not in info:
                raise RuntimeError('Created interface is not a P2P GO')
        subprocess.run([system_tool('ip'), 'address', 'add', '192.168.49.1/24', 'dev', iface], check=True)
        with permit_ap_dhcp(iface):
            dhcp = subprocess.Popen([system_tool('dnsmasq'), '--keep-in-foreground', '--conf-file=/dev/null',
                '--port=0', '--bind-interfaces', '--interface=' + iface,
                '--dhcp-range=192.168.49.2,192.168.49.254,255.255.255.0,15m',
                '--dhcp-option=3', '--dhcp-option=6', '--dhcp-authoritative',
                '--dhcp-leasefile=' + temp + '/leases', '--pid-file='],
                stdout=sys.stderr, stderr=sys.stderr)
            children.append(dhcp)
            time.sleep(.2)
            if dhcp.poll() is not None:
                raise RuntimeError('AP DHCP server exited')
            print('\t'.join([iface, ssid, password, str(frequency)]), flush=True)
            ready = True
            deadline = time.monotonic() + IDLE_LEASE_SECONDS
            last_bytes = ap_traffic_bytes(iface)
            while time.monotonic() < deadline and all(child.poll() is None for child in children):
                readable, _, _ = select.select([sys.stdin], [], [], 1)
                if readable:
                    chunk = sys.stdin.read(1)
                    if not chunk:
                        break
                    deadline = max(deadline, time.monotonic() + IDLE_LEASE_SECONDS)
                deadline, last_bytes = extend_lease_on_activity(
                    time.monotonic(), deadline, last_bytes, ap_traffic_bytes(iface)
                )
    except Exception as error:
        if ready:
            raise ActiveGoError('Active direct Wi-Fi host stopped: ' + str(error)) from error
        raise
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
        os.close(lock)
        if created:
            subprocess.run([system_tool('iw'), 'dev', iface, 'del'], check=True)


if __name__ == '__main__':
    if sys.argv[1:] == ['--self-test']:
        assert valid_parent('wlo1') and not any(valid_parent(x) for x in ('../wlo1', '.', '..', NAME, 'x' * 16))
        assert channel(5745) == 149 and channel(2462) == 11
        assert p2p_go_iface_name('wlo1', 7) == 'p2p-wlo1-7'
        assert p2p_go_iface_name('wlp0s20f3', 42) == 'p2p-wlp0s20f-42'
        assert len(p2p_go_iface_name('wlp0s20f3', 99)) <= 15
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
            parent, frequency = sys.argv[1], int(sys.argv[2])
            channel(frequency)
            try:
                run(parent, frequency, go=True)
            except ActiveGoError:
                raise
            except (RuntimeError, ValueError, GLib.Error, subprocess.SubprocessError) as error:
                print('Packet Lab P2P GO unavailable: ' + str(error), file=sys.stderr)
                run(parent, frequency)
        except Exception as exc:
            print('Packet Lab AP helper: ' + str(exc), file=sys.stderr)
            sys.exit(1)
