#!/usr/bin/python3
"""Host-only credential/DHCP trust-boundary tests; no network mutations."""
import importlib.util
from pathlib import Path
import unittest
from unittest.mock import Mock

spec = importlib.util.spec_from_file_location('client', Path(__file__).with_name('wifi-p2p-client-helper.py'))
client = importlib.util.module_from_spec(spec)
spec.loader.exec_module(client)


class ClientTests(unittest.TestCase):
    def test_busy_scan_is_deferred_then_targets_authenticated_group_channel(self):
        call = Mock()
        get = Mock(side_effect=[True, False])
        self.assertFalse(client.scan_group(call, get, '/station', b'DIRECT-test', 2467))
        call.assert_not_called()
        self.assertTrue(client.scan_group(call, get, '/station', b'DIRECT-test', 2467))
        options = call.call_args.args[-1][0]
        self.assertEqual(options['Channels'].unpack(), [(2467, 0)])
        self.assertEqual(bytes(options['SSIDs'].unpack()[0]), b'DIRECT-test')
        self.assertFalse(options['AllowRoam'].unpack())
        call.side_effect = RuntimeError('Scan rejected')
        with self.assertRaisesRegex(RuntimeError, 'Scan rejected'):
            client.scan_group(call, Mock(return_value=False), '/station', b'DIRECT-test', 0)

    def test_group_started_negotiated_address_is_used_without_dhcp(self):
        props = {'IpAddr': [192, 168, 49, 200], 'IpAddrMask': [255, 255, 255, 0],
                 'IpAddrGo': [192, 168, 49, 1]}
        gw = client.private_v4('192.168.49.1')
        self.assertEqual(client.negotiated_address(props, gw), '192.168.49.200/24')
        self.assertIsNone(client.negotiated_address({}, gw))
        props['IpAddrGo'] = [192, 168, 49, 9]
        with self.assertRaises(ValueError):
            client.negotiated_address(props, gw)
        with self.assertRaises(ValueError):
            client.negotiated_address({'IpAddr': [1]}, gw)

    def test_supplicant_receives_raw_psk_not_an_overlong_quoted_passphrase(self):
        props = client.group_properties(b'DIRECT-test', 'ab' * 32)
        self.assertEqual(props['psk'].get_type_string(), 'ay')
        self.assertEqual(bytes(props['psk'].unpack()), bytes.fromhex('ab' * 32))
        self.assertEqual(props['mode'].get_type_string(), 'i')
        self.assertEqual(props['mode'].unpack(), 0)

    def test_legacy_psk_matches_wpa2_known_vector(self):
        line = 'IEEE'.encode().hex() + '\t' + b'password'.hex() + '\t192.168.49.1\t2412\n'
        ssid, psk, gateway, frequency = client.credentials(line)
        self.assertEqual(ssid, b'IEEE')
        self.assertEqual(psk, 'f42c6fc52df0ebef9ebb4b90b38a5f902e83fe1b135a70e23aed762e9710a12e')
        self.assertEqual(str(gateway), '192.168.49.1')
        self.assertEqual(frequency, 2412)

    def test_invalid_credentials_fail_closed(self):
        for ssid, password, gw, frequency in [
            ('', 'password', '192.168.49.1', 2412),
            ('x' * 33, 'password', '192.168.49.1', 2412),
            ('DIRECT-test', 'short', '192.168.49.1', 2412),
            ('DIRECT-test', 'password', '8.8.8.8', 2412),
            ('DIRECT-test', 'password', '192.168.49.1', 2413),
        ]:
            with self.assertRaises(ValueError):
                client.credentials(f'{ssid.encode().hex()}\t{password.encode().hex()}\t{gw}\t{frequency}')

    def test_p2p_address_comes_from_authenticated_ssid_beacon(self):
        attr = bytes([13, 6, 0]) + bytes.fromhex('020000000001')
        ies = bytes([221, len(attr) + 4]) + bytes.fromhex('506f9a09') + attr
        self.assertEqual(client.p2p_device_address(ies), '02:00:00:00:00:01')
        self.assertIsNone(client.p2p_device_address(bytes([0, 1, 65])))
        for malformed in [bytes([221]), bytes([221, 20, 1]), ies[:-1]]:
            with self.assertRaises(ValueError):
                client.p2p_device_address(malformed)

    def test_dhcp_rejects_unrelated_subnet_and_unbounded_prefix(self):
        for address, mask in [('192.168.0.2', '255.255.255.0'), ('192.168.49.1', '255.255.255.0'),
                              ('192.168.49.2', '0.0.0.0'), ('8.8.8.8', '255.255.255.0')]:
            with self.assertRaises(ValueError):
                client.lease_address(address, mask, '192.168.49.1')


if __name__ == '__main__':
    unittest.main()
