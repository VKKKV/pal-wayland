# SPDX-License-Identifier: GPL-3.0-or-later
"""Authored resource fixtures; MZ contract metadata is explicitly mocked.

No game bytes or real acquisition manifest are read by these regressions.
"""
import copy
import hashlib
import importlib.util
import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile
from typing import Any
import unittest

TOOLS = Path(__file__).resolve().parents[1] / 'tools'
spec = importlib.util.spec_from_file_location('verify_assets', TOOLS / 'verify_assets.py')
if spec is None or spec.loader is None:
    raise RuntimeError('cannot load resource verifier')
verify_assets = importlib.util.module_from_spec(spec)
sys.path.insert(0, str(TOOLS))
try:
    spec.loader.exec_module(verify_assets)
finally:
    sys.path.pop(0)

# Only metadata contract tests use this driver. The file/ISO failure tests below
# invoke the actual CLI without replacing inspect_mz or any production checks.
MOCK_CLI = '''
import json
from pathlib import Path
import sys
from unittest.mock import patch
sys.path.insert(0, sys.argv[1])
import verify_assets
metadata = json.loads(sys.argv[3])
if metadata[0]['entry_hex'] == '<expected-prefix>':
    metadata[0]['entry_hex'] = verify_assets.RAW_ENTRY_PREFIX
with patch.object(verify_assets, 'inspect', side_effect=metadata):
    sys.exit(verify_assets.main(['--root', sys.argv[2]]))
'''
MODES = [('default', [], None), ('-O', ['-O'], None),
         ('PYTHONOPTIMIZE', [], '1')]


class VerifyAssetsTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory(prefix='verify-assets-', dir=os.environ['TMPDIR'])
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        (self.root / 'docs').mkdir()
        self.manifest: dict[str, Any] = {'files': [], 'unpacked_files': []}
        for folder, key, name, data in [
            ('dos-cd', 'files', 'PAL.EXE', bytes(range(64))),
            ('unpacked', 'unpacked_files', 'PAL.raw.exe', b'authored unpacked fixture'),
        ]:
            path = self.root / 'assets' / folder / name
            path.parent.mkdir(parents=True)
            path.write_bytes(data)
            self.manifest[key].append({'name': name, 'size': len(data),
                                       'sha256': hashlib.sha256(data).hexdigest()})
        iso_data = b'authored ISO hash fixture, not a CD image'
        (self.root / 'assets/GC050-track01.iso').write_bytes(iso_data)
        self.manifest['iso_sha256'] = hashlib.sha256(iso_data).hexdigest()
        self.write_manifest()
        self.metadata = [
            {'secondary_header': None, 'mz_header': {'relocations': 2376},
             'entry_file_offset': 0x264ac, 'entry_hex': '<expected-prefix>'},
            {'entry_file_offset': 128, 'pklite_marker_offset': 30},
        ]

    def write_manifest(self):
        (self.root / 'docs/resource-manifest.json').write_text(json.dumps(self.manifest))

    def run_cli(self, flags, optimize, metadata=None):
        env = dict(os.environ, PYTHONDONTWRITEBYTECODE='1')
        env.pop('PYTHONOPTIMIZE', None)
        if optimize is not None:
            env['PYTHONOPTIMIZE'] = optimize
        if metadata is None:
            command = [sys.executable, '-B', *flags, str(TOOLS / 'verify_assets.py'),
                       '--root', str(self.root)]
        else:
            command = [sys.executable, '-B', *flags, '-c', MOCK_CLI, str(TOOLS),
                       str(self.root), json.dumps(metadata)]
        return subprocess.run(command, env=env, capture_output=True, text=True,
                              cwd=self.root, timeout=10)

    def check_failure(self, diagnostic, metadata=None):
        for mode, flags, optimize in MODES:
            with self.subTest(mode=mode):
                result = self.run_cli(flags, optimize, metadata)
                self.assertEqual(result.returncode, 1, result.stdout + result.stderr)
                self.assertEqual(result.stdout, '')
                self.assertIn('FAIL:', result.stderr)
                self.assertIn(diagnostic, result.stderr)
                self.assertNotIn('PASS:', result.stdout + result.stderr)

    def test_runtime_inputs_and_cue_use_real_cli_without_intermediates(self):
        cd = self.root / 'assets/GC050.BIN'
        cd.write_bytes(b'authored complete disc')
        cue = cd.with_suffix('.cue')
        cue.write_text('REM user\'s legal archival copy\nFILE "GC050.BIN" BINARY\n TRACK 01 MODE1/2352\n INDEX 01 00:00:00\n')
        (self.root / 'docs/native-target.json').write_text(json.dumps(
            {'exe': 'assets/unpacked/PAL.raw.exe', 'assets': 'assets/dos-cd', 'cd_bin': 'assets/GC050.BIN'}))
        (self.root / 'docs/cd-audio-manifest.json').write_text(json.dumps(
            {'bytes': cd.stat().st_size, 'sha256': hashlib.sha256(cd.read_bytes()).hexdigest(),
             'track_layout': [[1, 'MODE1/2352', '00:00:00']]}))
        # Runtime precheck does not require ISO or intermediate unpack outputs.
        (self.root / 'assets/GC050-track01.iso').unlink()
        for mode, flags, optimize in MODES:
            env = dict(os.environ, PYTHONDONTWRITEBYTECODE='1')
            env.pop('PYTHONOPTIMIZE', None)
            if optimize is not None:
                env['PYTHONOPTIMIZE'] = optimize
            command = [sys.executable, '-B', *flags, str(TOOLS / 'verify_assets.py'),
                       '--root', str(self.root), '--runtime']
            with self.subTest(mode=mode):
                result = subprocess.run(command, env=env, capture_output=True, text=True, timeout=10)
                self.assertEqual(result.returncode, 0, result.stderr)
                self.assertIn('PASS: 4 supported build/runtime inputs', result.stdout)
                original = cue.read_text()
                # Distinct runtime-parser contracts, not interchangeable quoting.
                invalid = [original.replace('00:00:00', '00:00:01'),
                           original.replace('"GC050.BIN"', 'GC050.BIN'),
                           original.replace('TRACK', '"TRACK"'),
                           'REM ' + 'x' * 65536 + '\n' + original]
                for text in invalid:
                    cue.write_text(text)
                    bad = subprocess.run(command, env=env, capture_output=True, text=True, timeout=10)
                    self.assertEqual(bad.returncode, 1, bad.stderr)
                    self.assertEqual(bad.stdout, '')
                    self.assertIn('CUE', bad.stderr)
                cue.write_text(original)

    def test_mocked_metadata_success(self):
        for mode, flags, optimize in MODES:
            with self.subTest(mode=mode):
                result = self.run_cli(flags, optimize, self.metadata)
                self.assertEqual(result.returncode, 0, result.stderr)
                self.assertTrue(result.stdout.startswith('PASS: 2 files + ISO hash;'))
                self.assertEqual(result.stderr, '')

    def test_file_size_mismatch(self):
        for key, folder in [('files', 'dos-cd'), ('unpacked_files', 'unpacked')]:
            with self.subTest(key=key):
                item = self.manifest[key][0]
                item['size'] += 1
                self.write_manifest()
                self.check_failure(f'size mismatch: {self.root / "assets" / folder / item["name"]}')
                item['size'] -= 1
        self.write_manifest()

    def test_file_hash_mismatch_same_size(self):
        for key, folder in [('files', 'dos-cd'), ('unpacked_files', 'unpacked')]:
            with self.subTest(key=key):
                item = self.manifest[key][0]
                path = self.root / 'assets' / folder / item['name']
                original = path.read_bytes()
                path.write_bytes(bytes([original[0] ^ 1]) + original[1:])
                self.check_failure(f'hash mismatch: {path}')
                path.write_bytes(original)

    def test_iso_hash_mismatch(self):
        path = self.root / 'assets/GC050-track01.iso'
        path.write_bytes(b'corrupted authored ISO fixture')
        self.check_failure(f'hash mismatch: {path}')

    def test_all_mz_metadata_checks(self):
        cases = [
            (0, 'secondary_header', {}, 'secondary_header mismatch'),
            (0, 'relocations', 2375, 'relocations mismatch'),
            (0, 'entry_file_offset', 0, 'entry_file_offset mismatch'),
            (0, 'entry_hex', 'authored invalid prefix', 'entry_hex prefix mismatch'),
            (1, 'entry_file_offset', 0, 'entry_file_offset mismatch'),
            (1, 'pklite_marker_offset', -1, 'pklite_marker_offset mismatch'),
        ]
        for index, field, value, diagnostic in cases:
            with self.subTest(index=index, field=field):
                metadata = copy.deepcopy(self.metadata)
                target = metadata[index]['mz_header'] if field == 'relocations' else metadata[index]
                target[field] = value
                filename = 'unpacked/PAL.raw.exe' if index == 0 else 'dos-cd/PAL.EXE'
                self.check_failure(f'{diagnostic}: {self.root / "assets" / filename}', metadata)

    def test_invalid_mz_container(self):
        # Hashes and sizes match, but our authored bytes are not an MZ file.
        self.check_failure('not a DOS MZ executable')

    def test_missing_and_malformed_manifest(self):
        path = self.root / 'docs/resource-manifest.json'
        path.unlink()
        self.check_failure('resource-manifest.json')
        path.write_text('{')
        self.check_failure('Expecting property name')
        path.write_text('{}')
        self.check_failure('files')


if __name__ == '__main__':
    unittest.main()
