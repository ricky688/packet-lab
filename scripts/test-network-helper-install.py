#!/usr/bin/python3
"""Check authorization boundaries without installing or changing the network."""
import importlib.util
from pathlib import Path
import subprocess
import tempfile
import unittest
from unittest.mock import patch
import xml.etree.ElementTree as ET

spec = importlib.util.spec_from_file_location('installer', Path(__file__).with_name('install-network-helper.py'))
installer = importlib.util.module_from_spec(spec)
spec.loader.exec_module(installer)


class InstallTests(unittest.TestCase):
    def test_rule_only_allows_exact_helpers_for_the_selected_account(self):
        script = '''
const assert = require('assert');
let rule;
const polkit = {addRule: f => rule = f, Result: {YES: 'yes'}};
RULE
const subject = {user: 'ricky', active: true, local: true};
const action = {id: 'io.github.ricky688.PacketLab.network.ap',
  lookup: key => ({program: '/usr/lib/packet-lab-network/wifi-ap-helper.py', user: 'root'})[key]};
assert.strictEqual(rule(action, subject), 'yes');
assert.strictEqual(rule(action, {...subject, user: 'other'}), undefined);
assert.strictEqual(rule(action, {...subject, active: false, local: false}), 'yes');
assert.strictEqual(rule({...action, id: 'org.freedesktop.policykit.exec'}, subject), undefined);
assert.strictEqual(rule({...action, id: '__proto__'}, subject), undefined);
assert.strictEqual(rule({...action, lookup: k => k === 'program' ? '/usr/bin/python3' : 'root'}, subject), undefined);
assert.strictEqual(rule({...action, lookup: k => k === 'user' ? 'other' : action.lookup(k)}, subject), undefined);
assert.strictEqual(rule({id: 'io.github.ricky688.PacketLab.network.p2p-client',
  lookup: k => k === 'program' ? '/usr/lib/packet-lab-network/wifi-p2p-client-helper.py' : 'root'}, subject), 'yes');
'''.replace('RULE', installer.rule_for('ricky'))
        subprocess.run(['/usr/bin/node', '-e', script], check=True)

    def test_policy_matches_only_fixed_scripts_and_defaults_to_admin_auth(self):
        path = Path(__file__).parent.parent / 'data' / installer.POLICY.name
        actions = ET.parse(path).getroot().findall('action')
        self.assertEqual(len(actions), 2)
        for action, name in zip(actions, installer.HELPERS):
            annotation = action.find('annotate')
            self.assertEqual(annotation.attrib['key'], 'org.freedesktop.policykit.exec.path')
            self.assertEqual(annotation.text, str(installer.LIB / name))
            self.assertEqual(action.findtext('defaults/allow_any'), 'no')
            self.assertEqual(action.findtext('defaults/allow_inactive'), 'no')
            self.assertEqual(action.findtext('defaults/allow_active'), 'auth_admin')

    def test_user_controlled_directories_and_symlinks_are_rejected(self):
        installer.secure_directory(Path('/usr/lib'))
        with tempfile.TemporaryDirectory() as directory:
            with self.assertRaises(ValueError):
                installer.secure_directory(Path(directory))
            link = Path(directory) / 'link'
            link.symlink_to('/usr/lib', target_is_directory=True)
            with self.assertRaises(ValueError):
                installer.secure_directory(link)
            target = Path(directory) / 'file'
            with patch.object(installer, 'secure_directory'):
                installer.write_file(target, b'fixed helper', 0o755)
                self.assertEqual(target.read_bytes(), b'fixed helper')
                self.assertEqual(target.stat().st_mode & 0o777, 0o755)
                with self.assertRaises(ValueError):
                    installer.write_file(link, b'overwrite', 0o755)


if __name__ == '__main__':
    unittest.main()
