#!/usr/bin/env python3
# SPDX-License-Identifier: GPL-3.0-or-later
"""Statically inspect an MZ executable; never execute guest code."""
import argparse
import hashlib
import json
from pathlib import Path
import struct


def inspect(path):
    data = Path(path).read_bytes()
    if len(data) < 28 or data[:2] not in (b'MZ', b'ZM'):
        raise ValueError('not a DOS MZ executable')
    values = struct.unpack_from('<14H', data)
    names = ['magic', 'last_page_bytes', 'pages', 'relocations', 'header_paragraphs',
             'min_extra_paragraphs', 'max_extra_paragraphs', 'ss', 'sp', 'checksum',
             'ip', 'cs', 'relocation_table_offset', 'overlay_number']
    header = dict(zip(names, values))
    declared = (header['pages'] - 1) * 512 + (header['last_page_bytes'] or 512)
    header_bytes = header['header_paragraphs'] * 16
    if not header_bytes <= declared <= len(data):
        raise ValueError('invalid MZ size fields')
    # CS is a segment relative to load base. Packed files may use negative CS.
    entry = header_bytes + header['cs'] * 16 + header['ip']
    if entry >= declared:
        entry = header_bytes + (header['cs'] - 65536) * 16 + header['ip']
    if not header_bytes <= entry < declared:
        raise ValueError('entry outside load image')
    secondary = None
    if len(data) >= 64 and header_bytes >= 64:
        offset = struct.unpack_from('<I', data, 60)[0]
        if 64 <= offset <= len(data) - 4 and data[offset:offset+2] in (b'LE', b'LX', b'NE', b'PE'):
            secondary = {'offset': offset, 'signature_hex': data[offset:offset+4].hex()}
    reloc_end = header['relocation_table_offset'] + header['relocations'] * 4
    if header['relocations'] and reloc_end > header_bytes:
        raise ValueError('relocations outside header')
    return {'path': str(path), 'bytes': len(data), 'sha256': hashlib.sha256(data).hexdigest(),
            'mz_header': header, 'declared_file_bytes': declared,
            'secondary_header': secondary, 'entry_file_offset': entry,
            'entry_hex': data[entry:entry+64].hex(),
            'pklite_marker_offset': data.find(b'PKLITE'),
            'scope': 'Static container/entry inspection only; no complete code recovery or execution.'}


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument('exe', type=Path)
    args = parser.parse_args()
    print(json.dumps(inspect(args.exe), indent=2))


if __name__ == '__main__':
    main()
