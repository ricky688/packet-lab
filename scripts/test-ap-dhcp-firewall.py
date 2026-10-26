#!/usr/bin/python3
"""Root-only DHCP regression in a disposable network namespace; no host rules change."""
import importlib.util
import os
from pathlib import Path
import secrets
import socket
import struct
import subprocess
import sys
import tempfile
import time


def run(*args):
    return subprocess.run(args, check=True, stdout=subprocess.DEVNULL)


def discover(client_namespace, wait=1):
    mac = bytes.fromhex('020000000002')
    xid = secrets.token_bytes(4)
    bootp = struct.pack('!BBBB4sHH', 1, 1, 6, 0, xid, 0, 0x8000)
    bootp += bytes(16) + mac + bytes(10 + 64 + 128)
    bootp += bytes.fromhex('638253633501013703010306ff')
    udp = struct.pack('!HHHH', 68, 67, len(bootp) + 8, 0) + bootp
    ip = struct.pack('!BBHHHBBH4s4s', 0x45, 0, 20 + len(udp), 0, 0, 64, 17,
                     0, bytes(4), bytes([255] * 4))
    checksum = sum(struct.unpack('!10H', ip))
    while checksum >> 16:
        checksum = (checksum & 65535) + (checksum >> 16)
    ip = ip[:10] + struct.pack('!H', (~checksum) & 65535) + ip[12:]
    with open('/proc/self/ns/net') as original, open('/run/netns/' + client_namespace) as peer:
        os.setns(peer.fileno(), 0)
        try:
            client = socket.socket(socket.AF_PACKET, socket.SOCK_RAW, socket.htons(0x0800))
            client.bind(('probe0', 0x0800))
        finally:
            os.setns(original.fileno(), 0)
    with client:
        client.settimeout(.2)
        client.send(bytes([255] * 6) + mac + bytes.fromhex('0800') + ip + udp)
        end = time.monotonic() + wait
        while time.monotonic() < end:
            try:
                frame = client.recv(2048)
            except socket.timeout:
                continue
            if len(frame) < 282 or frame[23] != 17:
                continue
            offset = 14 + (frame[14] & 15) * 4
            payload = frame[offset + 8:]
            if frame[offset:offset + 4] == struct.pack('!HH', 67, 68) and payload[:1] == b'\x02' and payload[4:8] == xid:
                assert payload[16:19] == bytes([192, 168, 49]), 'Unexpected offered subnet'
                return True
    return False


def inside(client_namespace):
    spec = importlib.util.spec_from_file_location('ap_helper', Path(__file__).with_name('wifi-ap-helper.py'))
    helper = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(helper)
    run('ip', 'link', 'add', 'plabap0', 'type', 'veth', 'peer', 'name', 'probe0')
    run('ip', 'address', 'add', '192.168.49.1/24', 'dev', 'plabap0')
    run('ip', 'link', 'set', 'probe0', 'netns', client_namespace)
    run('ip', '-n', client_namespace, 'link', 'set', 'probe0', 'address', '02:00:00:00:00:02')
    run('ip', '-n', client_namespace, 'link', 'set', 'probe0', 'up')
    for name in ('lo', 'plabap0'):
        run('ip', 'link', 'set', name, 'up')
    run('iptables', '-A', 'INPUT', '-p', 'udp', '--dport', '67', '-j', 'DROP')
    with tempfile.TemporaryDirectory() as directory:
        log = open(directory + '/dhcp.log', 'w+')
        dhcp = subprocess.Popen(['dnsmasq', '--keep-in-foreground', '--conf-file=/dev/null',
            '--port=0', '--bind-interfaces', '--interface=plabap0',
            '--dhcp-range=192.168.49.2,192.168.49.254,255.255.255.0,5m',
            '--dhcp-option=3', '--dhcp-option=6', '--pid-file=', '--log-dhcp', '--log-facility=-',
            '--dhcp-leasefile=' + directory + '/leases'], stderr=log)
        try:
            end = time.monotonic() + 3
            while ':0043' not in Path('/proc/net/udp').read_text():
                assert dhcp.poll() is None and time.monotonic() < end, 'DHCP server not ready'
                time.sleep(.05)
            assert not discover(client_namespace), 'Baseline must reproduce blocked DHCP'
            with helper.permit_ap_dhcp():
                assert discover(client_namespace, wait=5), 'AP-scoped rule must allow a real DHCP offer'
            assert not discover(client_namespace), 'Temporary DHCP rule was not removed'
            print('Blocked DHCP -> valid offer -> blocked after cleanup: OK', flush=True)
        finally:
            dhcp.terminate()
            dhcp.wait(timeout=5)
            if sys.exc_info()[0] is not None:
                log.seek(0)
                print(log.read(), file=sys.stderr)
                subprocess.run(['iptables', '-nvL', 'INPUT'], check=True)
            log.close()


if __name__ == '__main__':
    assert os.geteuid() == 0, 'Run with system authorization'
    if len(sys.argv) == 3 and sys.argv[1] == '--inside':
        inside(sys.argv[2])
    else:
        assert len(sys.argv) == 1
        name = 'plab-dhcp-' + secrets.token_hex(4)
        run('ip', 'netns', 'add', name)
        try:
            run('ip', 'netns', 'add', name + '-client')
            try:
                subprocess.run(['ip', 'netns', 'exec', name, sys.executable,
                                str(Path(__file__).resolve()), '--inside', name + '-client'], check=True)
            finally:
                run('ip', 'netns', 'delete', name + '-client')
        finally:
            run('ip', 'netns', 'delete', name)
