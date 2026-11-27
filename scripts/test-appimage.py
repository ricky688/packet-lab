#!/usr/bin/env python3
"""Check AppImage integration without installing anything or launching sharing."""
import importlib.machinery
import importlib.util
import os
from pathlib import Path
import shutil
import subprocess
import sys
import tempfile
import unittest
from unittest.mock import Mock, patch
from gi.repository import GLib

SCRIPTS = Path(__file__).resolve().parent


class AppImageTests(unittest.TestCase):
    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory(prefix='Packet Lab ')
        self.addCleanup(self.temporary.cleanup)
        self.root = Path(self.temporary.name)
        self.usr = self.root / 'usr'
        self.usr.mkdir()
        self.image = self.root / 'Packet Lab.AppImage'
        self.image.touch()
        self.env = dict(os.environ, APPDIR=str(self.root), APPIMAGE=str(self.image),
                        XDG_DATA_HOME=str(self.root / 'data'), XDG_RUNTIME_DIR=str(self.root))

    def run_image(self, *args):
        return subprocess.run(['sh', str(SCRIPTS / 'appimage-AppRun'), *map(str, args)],
                              env=self.env, capture_output=True, text=True, timeout=5)

    def test_nemo_action_uses_persistent_image_and_can_be_removed(self):
        shutil.copyfile(SCRIPTS / 'install-nemo-action.py', self.usr / 'install-nemo-action')
        self.assertEqual(self.run_image('--install-nemo-action').returncode, 0)
        path = self.root / 'data/nemo/actions/packet-lab.nemo_action'
        action = GLib.KeyFile()
        action.load_from_file(str(path), GLib.KeyFileFlags.NONE)
        argv = GLib.shell_parse_argv(action.get_string('Nemo Action', 'Exec'))[1]
        self.assertEqual(argv, [str(self.image), '--send-files', '%F'])
        self.assertEqual(self.run_image('--uninstall-nemo-action').returncode, 0)
        self.assertFalse(path.exists())

    def test_network_helper_export_does_not_overwrite_existing_files(self):
        payload = self.usr / 'share/packet-lab'
        payload.mkdir(parents=True)
        names = ('install-network-helper.py', 'wifi-ap-helper.py', 'wifi-p2p-client-helper.py',
                 'io.github.ricky688.PacketLab.network.policy')
        for name in names:
            (payload / name).write_text(name)
        destination = self.root / 'network setup'
        self.assertEqual(self.run_image('--extract-network-helper', destination).returncode, 0)
        for name in names:
            self.assertEqual((destination / name).read_text(), name)
        self.assertEqual(self.run_image('--extract-network-helper', destination).returncode, 2)
        self.assertEqual((destination / names[0]).read_text(), names[0])

    def test_launcher_cleans_its_per_run_loader_cache(self):
        binary = self.usr / 'bin'
        binary.mkdir()
        query = binary / 'gdk-pixbuf-query-loaders'
        query.write_text('#!/bin/sh\nprintf "test loader cache\\n"\n')
        query.chmod(0o755)
        launcher = self.usr / 'packet-lab'
        launcher.write_text('#!/bin/sh\ntest -f "$GDK_PIXBUF_MODULE_FILE" || exit 1\nprintf "%s\\n" "$GDK_PIXBUF_MODULE_FILE"\n')
        launcher.chmod(0o755)
        result = self.run_image()
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertFalse(Path(result.stdout.strip()).exists())

    def test_sender_relaunches_the_image_instead_of_a_mount_path(self):
        loader = importlib.machinery.SourceFileLoader('packet_sender', str(SCRIPTS / 'packet-lab-send'))
        sender = importlib.util.module_from_spec(importlib.util.spec_from_loader(loader.name, loader))
        loader.exec_module(sender)
        file = self.root / 'file with spaces.txt'
        file.write_text('test')
        with patch.dict(os.environ, self.env), patch.object(sys, 'argv', ['packet-lab-send', str(file)]), \
             patch.object(sender.Gio, 'bus_get_sync', return_value=Mock()), \
             patch.object(sender.subprocess, 'Popen') as process:
            sender.main()
            self.assertEqual(process.call_args.args[0], [str(self.image)])

    def test_host_modes_use_real_go_or_hotspot_configuration(self):
        spec = importlib.util.spec_from_file_location('ap_modes', SCRIPTS / 'wifi-ap-helper.py')
        helper = importlib.util.module_from_spec(spec)
        spec.loader.exec_module(helper)
        go = helper.supplicant_config('DIRECT-test', 'testpassword', 2412, True)
        self.assertIn('p2p_disabled=0\n', go)
        self.assertIn('p2p_no_group_iface=1\n', go)
        self.assertIn('mode=3\n', go)
        self.assertIn('frequency=2412\n', go)
        ap = helper.supplicant_config('PacketLab-test', 'testpassword', 5620, False)
        self.assertIn('p2p_disabled=1\n', ap)
        self.assertIn('mode=2\n', ap)
        self.assertNotIn('p2p_no_group_iface', ap)

    def test_privileged_helpers_search_only_fixed_system_directories(self):
        for name in ('wifi-ap-helper.py', 'wifi-p2p-client-helper.py'):
            loader = importlib.machinery.SourceFileLoader(name, str(SCRIPTS / name))
            module = importlib.util.module_from_spec(importlib.util.spec_from_loader(loader.name, loader))
            loader.exec_module(module)
            with patch.object(module.os.path, 'isfile', side_effect=lambda path: path == '/usr/sbin/iw'), \
                 patch.object(module.os, 'access', return_value=True):
                self.assertEqual(module.system_tool('iw'), '/usr/sbin/iw')
            with patch.object(module.os.path, 'isfile', return_value=False):
                with self.assertRaises(FileNotFoundError):
                    module.system_tool('iw')


if __name__ == '__main__':
    unittest.main()
