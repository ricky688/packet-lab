#!/usr/bin/python3 -I
"""Install fixed, root-owned network helpers for one selected account."""
import argparse
import json
import os
from pathlib import Path
import pwd
import stat
import tempfile

LIB = Path('/usr/lib/packet-lab-network')
POLICY = Path('/usr/share/polkit-1/actions/io.github.ricky688.PacketLab.network.policy')
RULE = Path('/etc/polkit-1/rules.d/50-packet-lab-network.rules')
HELPERS = ('wifi-ap-helper.py', 'wifi-p2p-client-helper.py')


def secure_directory(path):
    for item in (path, *path.parents):
        mode = item.lstat()
        if not stat.S_ISDIR(mode.st_mode) or mode.st_uid != 0 or mode.st_mode & 0o022:
            raise ValueError(f'Unsafe system directory: {item}')


def write_file(path, data, mode):
    secure_directory(path.parent)
    if path.exists() or path.is_symlink():
        info = path.lstat()
        if not stat.S_ISREG(info.st_mode) or info.st_uid != 0 or info.st_mode & 0o022:
            raise ValueError(f'Unsafe existing system file: {path}')
    fd, temporary = tempfile.mkstemp(prefix='.packet-lab-', dir=path.parent)
    try:
        with os.fdopen(fd, 'wb') as stream:
            stream.write(data)
            os.fchmod(stream.fileno(), mode)
        os.replace(temporary, path)
    finally:
        if os.path.exists(temporary):
            os.unlink(temporary)


def rule_for(user):
    return '''// Only these root-owned network helpers; never authorize a generic interpreter.
polkit.addRule(function(action, subject) {
    var programs = {
        "io.github.ricky688.PacketLab.network.ap": "/usr/lib/packet-lab-network/wifi-ap-helper.py",
        "io.github.ricky688.PacketLab.network.p2p-client": "/usr/lib/packet-lab-network/wifi-p2p-client-helper.py"
    };
    if (Object.prototype.hasOwnProperty.call(programs, action.id) &&
        subject.user === USER &&
        action.lookup("program") === programs[action.id] && action.lookup("user") === "root") {
        return polkit.Result.YES;
    }
});
'''.replace('USER', json.dumps(user))


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    options = parser.add_mutually_exclusive_group(required=True)
    options.add_argument('--user', help='Account allowed to use these fixed network helpers')
    options.add_argument('--uninstall', action='store_true')
    args = parser.parse_args()
    if os.geteuid() != 0:
        raise PermissionError('Run this installer with system administrator authorization')
    if args.uninstall:
        for path in (RULE, POLICY, *(LIB / name for name in HELPERS)):
            secure_directory(path.parent)
            if path.exists() or path.is_symlink():
                if path.is_symlink() or path.lstat().st_uid != 0:
                    raise ValueError(f'Refusing to remove unsafe file: {path}')
                path.unlink()
        if LIB.exists():
            LIB.rmdir()
        print('Packet Lab network authorization removed')
        return
    user = pwd.getpwnam(args.user)
    if user.pw_uid == 0:
        raise ValueError('Select an ordinary local login, not root')
    source = Path(__file__).resolve().parent
    payloads = {name: (source / name).read_bytes() for name in HELPERS}
    if any(not data.startswith(b'#!/usr/bin/python3 -I\n') for data in payloads.values()):
        raise ValueError('Helpers must use isolated Python mode')
    policy_source = source / POLICY.name
    if not policy_source.is_file():
        policy_source = source.parent / 'data' / POLICY.name
    policy = policy_source.read_bytes()
    secure_directory(LIB.parent)
    LIB.mkdir(mode=0o755, exist_ok=True)
    secure_directory(LIB)
    for name, data in payloads.items():
        write_file(LIB / name, data, 0o755)
    write_file(POLICY, policy, 0o644)
    write_file(RULE, rule_for(user.pw_name).encode(), 0o644)
    print(f'Installed fixed network helpers for account {user.pw_name}')


if __name__ == '__main__':
    main()
