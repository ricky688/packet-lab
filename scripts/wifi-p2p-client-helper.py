#!/usr/bin/python3 -I
"""Join one authenticated legacy P2P group; never change station routes or DNS."""
import fcntl
import hashlib
import ipaddress
import os
from pathlib import Path
import re
import select
import subprocess
import sys
import time
from gi.repository import Gio, GLib

SERVICE = 'fi.w1.wpa_supplicant1'
ROOT = '/fi/w1/wpa_supplicant1'
P2P = SERVICE + '.Interface.P2PDevice'
NM = 'org.freedesktop.NetworkManager'
NMROOT = '/org/freedesktop/NetworkManager'


def private_v4(value):
    ip = ipaddress.IPv4Address(value)
    if not any(ip in ipaddress.IPv4Network(net) for net in
               ('10.0.0.0/8', '172.16.0.0/12', '192.168.0.0/16')):
        raise ValueError('Expected an RFC1918 IPv4 address')
    return ip


def credentials(line):
    ssid_hex, password_hex, gateway, frequency = line.rstrip('\n').split('\t')
    ssid, password = bytes.fromhex(ssid_hex), bytes.fromhex(password_hex)
    if not 1 <= len(ssid) <= 32 or not (8 <= len(password) <= 63 or
            len(password) == 64 and re.fullmatch(rb'[0-9a-fA-F]+', password)):
        raise ValueError('Invalid SSID or WPA2 credentials')
    frequency = int(frequency)
    if frequency != 0 and not (2412 <= frequency <= 2472 and (frequency - 2407) % 5 == 0
                              or 5180 <= frequency <= 5885 and frequency % 5 == 0):
        raise ValueError('Unsupported P2P frequency')
    psk = password.decode('ascii') if len(password) == 64 else hashlib.pbkdf2_hmac(
        'sha1', password, ssid, 4096, 32).hex()
    return ssid, psk, private_v4(gateway), frequency


def lease_address(address, mask, gateway):
    local, gateway = private_v4(address), private_v4(gateway)
    network = ipaddress.IPv4Network(f'{local}/{mask}', strict=False)
    if not 16 <= network.prefixlen <= 30 or gateway not in network or local in (
            gateway, network.network_address, network.broadcast_address):
        raise ValueError('DHCP lease is outside the authenticated gateway subnet')
    return f'{local}/{network.prefixlen}'


def group_properties(ssid, psk):
    # String PSKs are quoted by supplicant and treated as passphrases, not raw keys.
    return {'ssid': GLib.Variant('ay', ssid),
            'psk': GLib.Variant('ay', bytes.fromhex(psk)), 'mode': GLib.Variant('i', 0)}


def scan_group(call, get, path, ssid, frequency):
    if get(path, SERVICE + '.Interface', 'Scanning'):
        return False
    options = {'Type': GLib.Variant('s', 'active'),
               'SSIDs': GLib.Variant('aay', [ssid]), 'AllowRoam': GLib.Variant('b', False)}
    if frequency:
        options['Channels'] = GLib.Variant('a(uu)', [(frequency, 0)])
    call(path, SERVICE + '.Interface', 'Scan', '(a{sv})', (options,))
    return True


def negotiated_address(properties, gateway):
    keys = ('IpAddr', 'IpAddrMask', 'IpAddrGo')
    if not any(key in properties for key in keys):
        return None
    values = [bytes(properties.get(key, [])) for key in keys]
    if any(len(value) != 4 for value in values):
        raise ValueError('Incomplete negotiated P2P address')
    local, mask, owner = map(ipaddress.IPv4Address, values)
    if owner != gateway:
        raise ValueError('Negotiated GO address differs from authenticated gateway')
    return lease_address(str(local), str(mask), str(gateway))


def p2p_device_address(ies):
    data = bytearray()
    offset = 0
    while offset < len(ies):
        if offset + 2 > len(ies) or offset + 2 + ies[offset + 1] > len(ies):
            raise ValueError('Truncated Wi-Fi information element')
        kind, size = ies[offset:offset + 2]
        body = ies[offset + 2:offset + 2 + size]
        if kind == 221 and body[:4] == b'\x50\x6f\x9a\x09':
            data.extend(body[4:])
        offset += size + 2
    offset = 0
    while offset < len(data):
        if offset + 3 > len(data):
            raise ValueError('Truncated P2P attribute')
        kind, size = data[offset], int.from_bytes(data[offset + 1:offset + 3], 'little')
        if offset + 3 + size > len(data):
            raise ValueError('Truncated P2P attribute body')
        if kind in (3, 13, 15) and size >= 6:
            address = data[offset + 3:offset + 9]
            if not address[0] & 1 and any(address):
                return ':'.join(f'{byte:02X}' for byte in address)
        offset += size + 3
    return None


def run(parent, probe=False):
    if os.geteuid() != 0 or not re.fullmatch(r'[A-Za-z0-9_-]{1,15}', parent):
        raise ValueError('Requires root and a valid station interface')
    bus = Gio.bus_get_sync(Gio.BusType.SYSTEM, None)

    def call(path, interface, method, signature=None, args=(), service=SERVICE):
        try:
            result = bus.call_sync(service, path, interface, method,
                GLib.Variant(signature, args) if signature else None,
                None, Gio.DBusCallFlags.NONE, 5000, None).unpack()
        except GLib.Error as error:
            raise RuntimeError(f'{method}: {error.message}') from error
        return result[0] if len(result) == 1 else result

    def get(path, interface, prop):
        return call(path, 'org.freedesktop.DBus.Properties', 'Get', '(ss)', (interface, prop))

    def nmcall(path, interface, method, signature=None, args=()):
        return call(path, interface, method, signature, args, service=NM)

    def nmget(path, interface, prop):
        return nmcall(path, 'org.freedesktop.DBus.Properties', 'Get', '(ss)', (interface, prop))

    parent_path = call(ROOT, SERVICE, 'GetInterface', '(s)', (parent,))
    if get(parent_path, P2P, 'P2PDeviceConfig').get('NoGroupIface', False):
        raise RuntimeError('Supplicant cannot create a separate P2P interface')
    if probe:
        nmcall(NMROOT, NM, 'GetDevices')
        print('P2P client and NetworkManager APIs available', flush=True)
        return
    ssid, psk, gateway, frequency = credentials(sys.stdin.readline(1025))
    lock = os.open('/run/lock/packet-lab-ap.lock', os.O_CREAT | os.O_RDWR | os.O_NOFOLLOW, 0o600)
    if os.fstat(lock).st_uid != 0:
        raise RuntimeError('P2P lock is not root-owned')
    fcntl.flock(lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
    events = []

    def started(_bus, _sender, _path, _interface, _signal, params, *_data):
        properties = params.unpack()[0]
        if properties.get('role') == 'client':
            events.append(properties)

    subscription = bus.signal_subscribe(SERVICE, P2P, 'GroupStarted', None, None,
        Gio.DBusSignalFlags.NONE, started, None)
    profile = group_iface = nm_profile = nm_active = None
    discovery_started = False
    try:
        device = next(path for path in nmcall(NMROOT, NM, 'GetDevices')
                      if nmget(path, NM + '.Device', 'Interface') == 'p2p-dev-' + parent)
        try:
            nmcall(device, NM + '.Device.WifiP2P', 'StartFind', '(a{sv})', ({},))
            discovery_started = True
        except RuntimeError:
            pass  # Discovery may already be running for another request.
        scan_interfaces = [path for path in get(ROOT, SERVICE, 'Interfaces') if
                           get(path, SERVICE + '.Interface', 'Ifname') in (parent, 'p2p-dev-' + parent)]
        # Find the GO's P2P address from the exact authenticated SSID, never a Bluetooth MAC/name.
        end, peer = time.monotonic() + 8, None
        scan_sent, ssid_seen, address_seen = False, False, False
        scan_error = None
        while time.monotonic() < end:
            if not scan_sent:
                try:
                    scan_sent = scan_group(call, get, parent_path, ssid, frequency)
                except RuntimeError as error:
                    scan_error = str(error)
            bsses = [bss for path in scan_interfaces for bss in get(path, SERVICE + '.Interface', 'BSSs')]
            for bss in bsses:
                if bytes(get(bss, SERVICE + '.BSS', 'SSID')) != ssid:
                    continue
                ssid_seen = True
                address = p2p_device_address(bytes(get(bss, SERVICE + '.BSS', 'IEs')))
                if address:
                    address_seen = True
                    for candidate in nmget(device, NM + '.Device.WifiP2P', 'Peers'):
                        if nmget(candidate, NM + '.WifiP2PPeer', 'HwAddress').upper() == address:
                            peer = (candidate, address)
                            break
                if peer:
                    break
            if peer:
                break
            time.sleep(.1)
        if not peer:
            raise RuntimeError('P2P discovery incomplete: '
                f'scan_accepted={scan_sent}, ssid_seen={ssid_seen}, go_address_seen={address_seen}, '
                f'nm_peers={len(nmget(device, NM + ".Device.WifiP2P", "Peers"))}, '
                f'last_scan_error={scan_error}')
        nmcall(device, NM + '.Device.WifiP2P', 'StopFind')
        discovery_started = False
        settings = {
            'connection': {'id': GLib.Variant('s', 'PacketLab-P2P-legacy'),
                           'type': GLib.Variant('s', 'wifi-p2p'), 'autoconnect': GLib.Variant('b', False)},
            'wifi-p2p': {'peer': GLib.Variant('s', peer[1]), 'wps-method': GLib.Variant('u', 4)},
            'ipv4': {'method': GLib.Variant('s', 'auto'), 'never-default': GLib.Variant('b', True),
                     'ignore-auto-dns': GLib.Variant('b', True)},
            'ipv6': {'method': GLib.Variant('s', 'disabled')},
        }
        nm_profile, nm_active, _ = nmcall(NMROOT, NM, 'AddAndActivateConnection2',
            '(a{sa{sv}}ooa{sv})', (settings, device, peer[0], {'persist': GLib.Variant('s', 'volatile')}))
        end = time.monotonic() + 5
        while nmget(device, NM + '.Device', 'State') != 50:
            if time.monotonic() >= end or nmget(device, NM + '.Device', 'State') >= 110:
                raise RuntimeError('NetworkManager did not enter P2P configuration')
            time.sleep(.05)
        # Let NM own the group, then replace its WPS attempt with the pre-provisioned PSK join.
        time.sleep(.1)
        call(parent_path, P2P, 'Cancel')
        profile = call(parent_path, P2P, 'AddPersistentGroup', '(a{sv})', (group_properties(ssid, psk),))
        args = {'persistent_group_object': GLib.Variant('o', profile)}
        if frequency:
            args['frequency'] = GLib.Variant('i', frequency)
        call(parent_path, P2P, 'GroupAdd', '(a{sv})', (args,))
        end = time.monotonic() + 20
        iface = None
        properties = None
        while time.monotonic() < end:
            context = GLib.MainContext.default()
            while context.pending():
                context.iteration(False)
            for event in events:
                if bytes(get(event['group_object'], SERVICE + '.Group', 'SSID')) == ssid:
                    group_iface = event['interface_object']
                    iface = get(group_iface, SERVICE + '.Interface', 'Ifname')
                    properties = event
                    break
            if iface:
                break
            time.sleep(.1)
        if not iface or not re.fullmatch(r'p2p-[A-Za-z0-9_.-]{1,11}', iface) or iface == parent:
            raise RuntimeError('No separate P2P-client interface became associated')
        info = subprocess.run(['/usr/bin/iw', 'dev', iface, 'info'], capture_output=True,
                              text=True, check=True).stdout
        if 'type P2P-client' not in info:
            raise RuntimeError('Joined interface is not a P2P client')
        expected = negotiated_address(properties, gateway)
        end = time.monotonic() + 12
        while time.monotonic() < end:
            if nmget(nm_active, NM + '.Connection.Active', 'State') == 2:
                config = nmget(nm_active, NM + '.Connection.Active', 'Ip4Config')
                if config != '/':
                    for row in nmget(config, NM + '.IP4Config', 'AddressData'):
                        try:
                            address = lease_address(row['address'], str(row['prefix']), str(gateway))
                        except ValueError:
                            continue
                        if expected is None or address == expected:
                            break
                    else:
                        time.sleep(.1)
                        continue
                    break
            time.sleep(.1)
        else:
            raise RuntimeError('NetworkManager P2P address did not become ready')
        print('READY\t' + iface, flush=True)
        # The app closes this pipe on completion, failure, or process exit.
        select.select([sys.stdin], [], [])
    finally:
        bus.signal_unsubscribe(subscription)
        if discovery_started:
            try:
                nmcall(device, NM + '.Device.WifiP2P', 'StopFind')
            except RuntimeError:
                pass
        if nm_active:
            try:
                nmcall(NMROOT, NM, 'DeactivateConnection', '(o)', (nm_active,))
            except RuntimeError:
                pass
        if group_iface:
            try:
                call(group_iface, P2P, 'Disconnect')
            except RuntimeError:
                pass
        elif profile:
            try:
                call(parent_path, P2P, 'Cancel')
            except RuntimeError:
                pass
        if profile:
            # No SaveConfig: this group only ever existed in supplicant memory.
            call(parent_path, P2P, 'RemovePersistentGroup', '(o)', (profile,))
        if nm_profile:
            try:
                nmcall(nm_profile, NM + '.Settings.Connection', 'Delete')
            except RuntimeError:
                pass
        os.close(lock)


if __name__ == '__main__':
    try:
        run(sys.argv[-1], probe=sys.argv[1] == '--probe')
    except Exception as error:
        # Never print the request or credentials on failure.
        print(f'P2P client helper: {type(error).__name__}: {error}', file=sys.stderr)
        sys.exit(1)
