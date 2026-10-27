#!/usr/bin/env python3
"""Install/remove the current bundle's Nemo right-click action for this user."""
import argparse
import os
from pathlib import Path
from gi.repository import GLib


def action_text(helper, appimage=False):
    action = GLib.KeyFile()
    values = {
        'Name': 'Send with Packet Lab',
        'Comment': 'Choose a nearby device to receive the selected files',
        'Exec': GLib.shell_quote(str(helper)).replace('%', '%%') + (' --send-files %F' if appimage else ' %F'),
        'Selection': 'notnone', 'Extensions': 'nodirs;', 'UriScheme': 'file',
        'Quote': 'double', 'Icon-Name': 'document-send',
    }
    for key, value in values.items():
        action.set_string('Nemo Action', key, value)
    return action.to_data()[0]


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--uninstall', action='store_true')
    parser.add_argument('--self-test', action='store_true')
    args = parser.parse_args()
    if args.self_test:
        helper = Path('/tmp/a bundle/packet-lab-send')
        action = GLib.KeyFile()
        data = action_text(helper)
        action.load_from_data(data, len(data.encode()), GLib.KeyFileFlags.NONE)
        command = action.get_string('Nemo Action', 'Exec')
        assert GLib.shell_parse_argv(command)[1] == [str(helper), '%F']
        assert action.get_string('Nemo Action', 'Extensions') == 'nodirs;'
        assert action.get_string('Nemo Action', 'UriScheme') == 'file'
        data = action_text(Path('/tmp/Packet Lab.AppImage'), appimage=True)
        action.load_from_data(data, len(data.encode()), GLib.KeyFileFlags.NONE)
        assert GLib.shell_parse_argv(action.get_string('Nemo Action', 'Exec'))[1] == [
            '/tmp/Packet Lab.AppImage', '--send-files', '%F']
        print('Nemo action validation: OK')
        return
    target = Path(os.environ.get('XDG_DATA_HOME', str(Path.home() / '.local/share'))) / 'nemo/actions/packet-lab.nemo_action'
    if args.uninstall:
        target.unlink(missing_ok=True)
        print('Removed Packet Lab Nemo action')
        return
    root = Path(__file__).resolve().parent
    image = os.environ.get('APPIMAGE')
    helper = Path(image).resolve() if image else root / 'packet-lab-send'
    if not helper.is_file() or (not image and not (root / 'packet-lab').is_file()):
        parser.error('Run install-nemo-action from the extracted native bundle')
    target.parent.mkdir(parents=True, exist_ok=True)
    target.write_text(action_text(helper, appimage=bool(image)))
    print(f'Installed {target}; reopen the Nemo context menu to use it')


if __name__ == '__main__':
    main()
