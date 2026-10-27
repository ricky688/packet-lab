#!/usr/bin/env python3
"""Assemble an AppDir inside the Ubuntu baseline container, never from host libs."""
import hashlib
import json
import os
from pathlib import Path
import re
import shutil
import subprocess

source, work = Path('/source'), Path('/work')
runtime = work / 'runtime'
version = __import__('tomllib').loads((source / 'Cargo.toml').read_text())['package']['version']
bundle = source / f'dist/packet-lab-{version}-linux-x86_64'
appdir = work / 'PacketLab.AppDir'
if appdir.exists():
    shutil.rmtree(appdir)
usr = appdir / 'usr'
(usr / 'bin').mkdir(parents=True)
for name in ('bin/packet-lab', 'packet-lab'):
    shutil.copy2(bundle / name, usr / name)
for name in ('share/glib-2.0/schemas', 'share/icons'):
    shutil.copytree(bundle / name, usr / name)
payload = usr / 'share/packet-lab'
payload.mkdir()
for name in ('resources.gresource', 'ui-resources.gresource'):
    shutil.copyfile(bundle / 'share/packet-lab' / name, payload / name)
for name in ('wifi-ap-helper.py', 'wifi-p2p-client-helper.py', 'install-network-helper.py'):
    shutil.copyfile(source / 'scripts' / name, payload / name)
shutil.copyfile(source / 'data/io.github.ricky688.PacketLab.network.policy',
                payload / 'io.github.ricky688.PacketLab.network.policy')
(usr / 'licenses').mkdir()
for origin, target in (('LICENSE', 'Packet-GPL-3.0.txt'),
                       ('subprojects/rqs_lib/LICENSE', 'rquickshare-GPL-3.0.txt')):
    shutil.copyfile(source / origin, usr / 'licenses' / target)
lib = usr / 'lib'
lib.mkdir()
(lib / 'gio/modules').mkdir(parents=True)
for name in ('packet-lab-send', 'install-nemo-action.py'):
    target = 'install-nemo-action' if name.endswith('.py') else name
    shutil.copyfile(source / 'scripts' / name, usr / target)
    (usr / target).chmod(0o755)
shutil.copyfile(source / 'scripts/appimage-AppRun', appdir / 'AppRun')
(appdir / 'AppRun').chmod(0o755)
for name in ('README.md', 'CHANGELOG.md', 'UPSTREAM.md'):
    shutil.copyfile(source / name, usr / name)
# Copy only public documentation; local bundles and docs contain private diagnostics.
public_docs = ('HOTSPOT-HANDOVER.md', 'APPIMAGE.md')
(usr / 'docs').mkdir()
for name in public_docs:
    shutil.copyfile(source / 'docs' / name, usr / 'docs' / name)
assert sorted(path.name for path in (usr / 'docs').iterdir()) == sorted(public_docs)
loaders = lib / 'gdk-pixbuf-loaders'
loaders.mkdir()
for path in Path('/usr/lib/x86_64-linux-gnu/gdk-pixbuf-2.0/2.10.0/loaders').glob('*.so'):
    shutil.copyfile(path, loaders / path.name)
query = Path('/usr/lib/x86_64-linux-gnu/gdk-pixbuf-2.0/gdk-pixbuf-query-loaders')
shutil.copy2(query, usr / 'bin/gdk-pixbuf-query-loaders')
os.environ['LD_LIBRARY_PATH'] = str(runtime / 'lib')
excluded = re.compile(r'^(?:ld-linux.*|lib(?:c|m|dl|pthread|rt|resolv)\.so\..*)$')
manifest = {}
for binary in [usr / 'bin/packet-lab', usr / 'bin/gdk-pixbuf-query-loaders', *loaders.glob('*.so')]:
    deps = subprocess.check_output(['ldd', str(binary)], text=True)
    if 'not found' in deps:
        raise RuntimeError(deps)
    for name, origin in re.findall(r'^\s*(\S+) => (/\S+)', deps, re.MULTILINE):
        if excluded.match(name) or name in manifest:
            continue
        shutil.copyfile(origin, lib / name)
        subprocess.run(['patchelf', '--set-rpath', '$ORIGIN', str(lib / name)], check=True)
        manifest[name] = {'origin': origin, 'sha256': hashlib.sha256((lib / name).read_bytes()).hexdigest()}
for binary in (usr / 'bin').iterdir():
    if binary.read_bytes()[:4] == b'\x7fELF':
        subprocess.run(['patchelf', '--set-rpath', '$ORIGIN/../lib', str(binary)], check=True)
schemas = usr / 'share/glib-2.0/schemas'
for path in (runtime / 'share/glib-2.0/schemas').glob('*.xml'):
    shutil.copyfile(path, schemas / path.name)
subprocess.run([str(runtime / 'bin/glib-compile-schemas'), str(schemas)], check=True)
fonts = usr / 'share/fonts/truetype/dejavu'
fonts.mkdir(parents=True)
shutil.copyfile('/usr/share/fonts/truetype/dejavu/DejaVuSans.ttf', fonts / 'DejaVuSans.ttf')
fontconfig = usr / 'etc/fonts'
fontconfig.mkdir(parents=True)
(fontconfig / 'fonts.conf').write_text('''<?xml version="1.0"?>
<!DOCTYPE fontconfig SYSTEM "urn:fontconfig:fonts.dtd">
<fontconfig><include ignore_missing="yes">/etc/fonts/fonts.conf</include>
<dir prefix="relative">../../share/fonts</dir><cachedir prefix="xdg">fontconfig</cachedir></fontconfig>
''')
icon = 'io.github.ricky688.PacketLab'
shutil.copyfile(source / f'data/icons/{icon}.svg', appdir / f'{icon}.svg')
(appdir / '.DirIcon').symlink_to(f'{icon}.svg')
(appdir / f'{icon}.desktop').write_text(f'''[Desktop Entry]
Type=Application
Name=Packet Lab
Exec=AppRun
Icon={icon}
Categories=Utility;Network;
Terminal=false
StartupNotify=true
DBusActivatable=false
''')
licenses = usr / 'licenses/runtime'
licenses.mkdir(parents=True)
shutil.copytree('/usr/share/doc', licenses / 'ubuntu-notices', ignore=lambda directory, names: [n for n in names if n != 'copyright' and not (Path(directory) / n).is_dir()])
sources = usr / 'sources'
sources.mkdir()
for path in [*(work / 'downloads').glob('*.tar.xz'), *(work / 'downloads').glob('*.tar.gz')]:
    shutil.copyfile(path, sources / path.name)
shutil.copyfile(work / 'packet-lab-source.tar.gz', sources / 'packet-lab-source.tar.gz')
shutil.copyfile(source / 'scripts/appimage-sources.json', sources / 'runtime-sources.json')
installed = subprocess.check_output(['dpkg-query', '-W', '-f=${binary:Package}\t${Version}\t${source:Package}\t${source:Version}\n'], text=True)
(licenses / 'ubuntu-package-versions.tsv').write_text(installed)
(licenses / 'SOURCE-ACCESS.txt').write_text('Unmodified Ubuntu library sources: https://launchpad.net/ubuntu/+source/PACKAGE/VERSION using the source package/version columns in ubuntu-package-versions.tsv. GNOME runtime and Packet Lab source archives are included in usr/sources.\n')
requirements = set()
for path in [usr / 'bin/packet-lab', *lib.glob('*.so*')]:
    versions = subprocess.check_output(['readelf', '--version-info', str(path)], text=True)
    requirements.update(re.findall(r'Name: GLIBC_(\d+\.\d+(?:\.\d+)?)', versions))
maximum = max(requirements, key=lambda v: tuple(map(int, v.split('.'))))
assert tuple(map(int, maximum.split('.'))) <= (2, 39), maximum
metadata = usr / 'share/packet-lab/APPIMAGE-BUILD.json'
metadata.write_text(json.dumps({'version': version, 'source_revision': (work / 'source-revision.txt').read_text().strip(), 'architecture': 'x86_64', 'glibc_minimum': maximum,
    'gtk': '4.22.0', 'libadwaita': '1.9.0', 'libraries': manifest}, indent=2) + '\n')
(usr / 'BUILD-INFO.txt').write_text(f'Packet Lab {version} AppImage alpha\nx86_64, glibc >= {maximum}\nGTK 4.22.0 / libadwaita 1.9.0 built on Ubuntu 24.04\nBlueZ, NetworkManager, polkit, kernel drivers and optional host Python helpers remain system services.\n')
subprocess.run(['desktop-file-validate', str(appdir / f'{icon}.desktop')], check=True)
print(f'AppDir ready: {len(manifest)} bundled libraries; maximum GLIBC requirement {maximum}')
