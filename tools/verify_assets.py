#!/usr/bin/env python3
# SPDX-License-Identifier: GPL-3.0-or-later
"""Verify locally obtained resources against their acquisition manifest."""
import argparse
import hashlib
import json
import re
from pathlib import Path
import sys

from inspect_mz import inspect

ROOT = Path(__file__).resolve().parents[1]
RAW_ENTRY_PREFIX = 'b430cd213c02'


def require(condition, message):
    """Resource checks must remain active under Python's optimization flags."""
    if not condition:
        raise ValueError(message)


def file_identity(path, item):
    path = Path(path)
    require(path.stat().st_size == item['size'], f'size mismatch: {path}')
    with path.open('rb') as stream:
        require(hashlib.file_digest(stream, 'sha256').hexdigest() == item['sha256'],
                f'hash mismatch: {path}')


def verify_runtime(root, target=None):
    """Check supported inputs before compilation; no ISO/intermediate files needed."""
    root = Path(root)
    if target is None:
        target = json.loads((root / 'docs/native-target.json').read_text())
    manifest = json.loads((root / 'docs/resource-manifest.json').read_text())
    cd_manifest = json.loads((root / 'docs/cd-audio-manifest.json').read_text())
    raw = next((item for item in manifest['unpacked_files'] if item['name'] == 'PAL.raw.exe'), None)
    require(raw is not None, 'manifest missing PAL.raw.exe identity')
    file_identity(root / target['exe'], raw)
    for item in manifest['files']:
        file_identity(root / target['assets'] / item['name'], item)
    cd = root / target['cd_bin']
    file_identity(cd, {'size': cd_manifest['bytes'], 'sha256': cd_manifest['sha256']})
    # Accept whitespace/line-ending differences, not a different disc layout.
    expected = [['FILE', cd.name, 'BINARY']]
    for number, mode, index in cd_manifest['track_layout']:
        expected.extend([['TRACK', f'{number:02d}', mode], ['INDEX', '01', index]])
    cue = cd.with_suffix('.cue')
    with cue.open('rb') as stream:
        data = stream.read(65537)
    require(len(data) <= 65536, f'CUE exceeds size limit: {cue}')
    actual = []
    for line in data.decode('utf-8').splitlines():
        tokens = line.split()
        if not tokens or tokens[0] == 'REM':
            continue
        if tokens[0] == 'FILE':
            name = re.fullmatch(r'"([^"]*)"\s*BINARY', line.strip()[4:].strip())
            require(name is not None, f'unsupported CUE FILE syntax: {cue}')
            tokens = ['FILE', name[1], 'BINARY']
        actual.append(tokens)
    require(actual == expected, f'unsupported CUE track layout: {cd.with_suffix(".cue")}')
    return len(manifest['files']) + 3  # Data files, raw EXE, complete BIN and CUE.


def verify(root):
    root = Path(root)
    manifest = json.loads((root / 'docs/resource-manifest.json').read_text())
    count = 0
    for folder, key in [('dos-cd', 'files'), ('unpacked', 'unpacked_files')]:
        for item in manifest[key]:
            p = root / 'assets' / folder / item['name']
            data = p.read_bytes()
            require(len(data) == item['size'], f'size mismatch: {p}')
            require(hashlib.sha256(data).hexdigest() == item['sha256'], f'hash mismatch: {p}')
            count += 1
    iso = root / 'assets/GC050-track01.iso'
    require(hashlib.sha256(iso.read_bytes()).hexdigest() == manifest['iso_sha256'],
            f'hash mismatch: {iso}')
    raw_path = root / 'assets/unpacked/PAL.raw.exe'
    raw = inspect(raw_path)
    require(raw['secondary_header'] is None, f'secondary_header mismatch: {raw_path}')
    require(raw['mz_header']['relocations'] == 2376, f'relocations mismatch: {raw_path}')
    require(raw['entry_file_offset'] == 0x264ac, f'entry_file_offset mismatch: {raw_path}')
    require(raw['entry_hex'].startswith(RAW_ENTRY_PREFIX), f'entry_hex prefix mismatch: {raw_path}')
    packed_path = root / 'assets/dos-cd/PAL.EXE'
    packed = inspect(packed_path)
    require(packed['entry_file_offset'] == 128, f'entry_file_offset mismatch: {packed_path}')
    require(packed['pklite_marker_offset'] == 30, f'pklite_marker_offset mismatch: {packed_path}')
    return count


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--root', type=Path, default=ROOT,
                        help='resource repository root (default: this repository)')
    parser.add_argument('--runtime', action='store_true',
                        help='only build/runtime inputs, including complete BIN and CUE')
    args = parser.parse_args(argv)
    try:
        count = verify_runtime(args.root) if args.runtime else verify(args.root)
    except (OSError, ValueError, KeyError, TypeError) as exc:
        print(f'FAIL: {exc}', file=sys.stderr)
        return 1
    if args.runtime:
        print(f'PASS: {count} supported build/runtime inputs; complete BIN and CUE layout.')
    else:
        print(f'PASS: {count} files + ISO hash; packed/unpacked MZ headers and actual entry prefix.')
    return 0


if __name__ == '__main__':
    sys.exit(main())
