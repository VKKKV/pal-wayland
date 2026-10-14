#!/usr/bin/env python3
# SPDX-License-Identifier: GPL-3.0-or-later
"""Original fixtures only; real PAL startup is opt-in via PAL_ORACLE_EXE.

  /path/to/dev-venv/bin/python -B -m unittest discover -s tests -p test_oracle_trace.py -v
  PAL_ORACLE_EXE=/local/PAL.raw.exe /path/to/dev-venv/bin/python -B ...

No original game code/data is embedded. Temporary fixtures require $TMPDIR.
CPU tests skip if dev dependencies are absent;
loader, manifest and output-policy tests still run with the standard library.
"""
import contextlib
import argparse
import hashlib
import importlib.util
import io
import json
import os
from pathlib import Path
import struct
import subprocess
import sys
import tempfile
import unittest

TOOL = Path(__file__).resolve().parents[1] / 'tools' / 'oracle_trace.py'
spec = importlib.util.spec_from_file_location('pal_oracle_trace', TOOL)
assert spec is not None and spec.loader is not None
oracle = importlib.util.module_from_spec(spec)
sys.modules[spec.name] = oracle
spec.loader.exec_module(oracle)
HAS_CPU = importlib.util.find_spec('unicorn') is not None and importlib.util.find_spec('capstone') is not None


def fixture(code=b'\x90', *, relocations=(), image_size=512, **fields):
    """Entirely original plain MZ, with 512 bytes of zeroed code/stack space."""
    header_size = ((28 + len(relocations) * 4 + 15) // 16) * 16
    image_size = max(image_size, len(code))
    total = header_size + image_size
    words = dict(magic=0x5a4d, last=total % 512, pages=(total + 511) // 512,
                 count=len(relocations), paragraphs=header_size // 16, minimum=0,
                 maximum=0xffff, ss=0, sp=image_size, checksum=0, ip=0, cs=0,
                 table=28, overlay=0)
    words.update(fields)
    data = bytearray(total)
    struct.pack_into('<14H', data, 0, *words.values())
    for index, (offset, segment) in enumerate(relocations):
        struct.pack_into('<HH', data, 28 + index * 4, offset, segment)
    data[header_size:header_size + len(code)] = code
    return bytes(data)


def run_code(code, *, registers=None, stops=None, **options):
    loaded = oracle.load_mz(fixture(code))
    if registers:
        oracle.initial_state(loaded, {'registers': registers})
    machine = oracle.Oracle(loaded)
    trace = []
    result = machine.run(stops=stops if stops is not None else [(0x1010, len(code))],
                         emit=trace.append, **options)
    return machine, trace, result


class LoaderTests(unittest.TestCase):
    def test_environment_allocation_defaults(self):
        loaded = oracle.load_mz(fixture(b'\x90', minimum=3))
        self.assertEqual(len(loaded.memory), 1 << 20)
        self.assertEqual(loaded.metadata['allocation_bytes'], 560)
        self.assertEqual(loaded.memory[0x10100], 0x90)
        self.assertEqual(loaded.registers, dict(ax=0, bx=0, cx=0, dx=0, si=0, di=0,
                                              bp=0, sp=512, cs=0x1010, ip=0, ds=0x1000,
                                              es=0x1000, ss=0x1010, flags=0x202))
        self.assertEqual(loaded.memory[0x10000:0x10004], b'\xcd\x20\x00\x9f')
        self.assertEqual(loaded.memory[0x1002c:0x1002e], b'\x00\x08')
        self.assertEqual(loaded.memory[0x10080:0x10082], b'\0\r')
        env = b'PATH=C:\\\0\0\x01\0C:\\PAL.EXE\0'
        self.assertEqual(loaded.memory[0x8000:0x8000 + len(env)], env)
        self.assertEqual(struct.unpack_from('<H', loaded.memory, 0x413)[0], 636)

    def test_ordered_duplicate_overlapping_relocations(self):
        data = fixture(b'\xfe\xfe\xff\0', relocations=[(0, 0), (1, 0), (0, 0)])
        loaded = oracle.load_mz(data)
        expected = bytearray(b'\xfe\xfe\xff\0')
        for offset in (0, 1, 0):
            lo = expected[offset] + (0x1010 & 0xff)
            hi = expected[offset + 1] + (0x1010 >> 8) + (lo >> 8)
            expected[offset], expected[offset + 1] = lo & 255, hi & 255
        self.assertEqual(loaded.memory[0x10100:0x10104], expected)
        self.assertEqual(loaded.metadata['relocations'], 3)
        self.assertEqual(data, fixture(b'\xfe\xfe\xff\0', relocations=[(0, 0), (1, 0), (0, 0)]))

    def test_all_2376_relocations_not_a_prefix(self):
        image = b'\x01\x00' * 2376
        records = [(index * 2, 0) for index in range(2376)]
        loaded = oracle.load_mz(fixture(image, relocations=records, image_size=8192))
        actual = loaded.memory[0x10100:0x10100 + len(image)]
        self.assertEqual(actual, b'\x11\x10' * 2376)
        self.assertEqual(loaded.metadata['relocations'], 2376)

    def test_truncated_and_malformed_headers_fail(self):
        good = fixture(relocations=[(0, 0)])
        for end in range(len(good)):
            with self.subTest(end=end), self.assertRaises(oracle.OracleError):
                oracle.load_mz(good[:end])
        for changes in ({'magic': 0}, {'pages': 0}, {'last': 512}, {'paragraphs': 1},
                        {'minimum': 2, 'maximum': 1}, {'overlay': 1}, {'ip': 512},
                        {'table': 27}, {'table': 31}, {'sp': 1}, {'ss': 0xffff},
                        {'minimum': 0xffff}, {'cs': 0xfffe}):
            with self.subTest(changes=changes), self.assertRaises(oracle.OracleError):
                oracle.load_mz(fixture(relocations=[(0, 0)], **changes))
        for segment in (-1, 0, 0x1000, 0x9f00, 0x10000):
            with self.assertRaises(oracle.OracleError):
                oracle.load_mz(good, segment)
        for record in ((511, 0), (0, 0x20), (65535, 65535)):
            with self.assertRaises(oracle.OracleError):
                oracle.load_mz(fixture(relocations=[record]))

    def test_tail_is_not_loaded_padding_zero_and_zm_supported(self):
        data = fixture(image_size=17, sp=16)
        data = b'ZM' + data[2:] + b'not loaded'
        loaded = oracle.load_mz(data)
        self.assertEqual(loaded.memory[0x10111:0x10120], bytes(15))
        self.assertEqual(loaded.metadata['image_bytes'], 17)
        full_page = fixture(image_size=480)
        self.assertEqual(oracle.load_mz(full_page).metadata['declared_bytes'], 512)

    def test_negative_cs_and_zero_sp(self):
        loaded = oracle.load_mz(fixture(cs=0xfff0, ip=0x100))
        self.assertEqual((loaded.registers['cs'], loaded.registers['ip']), (0x1000, 0x100))
        loaded = oracle.load_mz(fixture(sp=0, minimum=4064))
        self.assertEqual(loaded.registers['sp'], 0)
        with self.assertRaises(oracle.OracleError):
            oracle.load_mz(fixture(sp=0))

    def test_manifest_hash_check_with_original_fixture(self):
        data = fixture()
        entry = dict(name='fixture.exe', size=len(data), sha256=hashlib.sha256(data).hexdigest())
        self.assertEqual(oracle.verify_manifest(data, {'unpacked_files': [entry]}), entry)
        with self.assertRaises(oracle.OracleError):
            oracle.verify_manifest(data + b'x', {'unpacked_files': [entry]})
        with self.assertRaises(oracle.OracleError):
            oracle.verify_manifest(data, {'unpacked_files': [{**entry, 'size': 0}]})

    def test_initial_state_rejects_unknown_keys_and_out_of_range(self):
        loaded = oracle.load_mz(fixture())
        oracle.initial_state(loaded, {'registers': {'ax': '0xffff', 'ip': 5}, 'defined_flags': 0})
        self.assertEqual(loaded.registers['ax'], 65535)
        for state in ([], {'ax': 5}, {'registers': {'eax': 1}}, {'registers': {'ax': -1}},
                      {'registers': {'ax': True}}, {'registers': {'flags': 0x100}},
                      {'registers': {'flags': 0x8000}}, {'defined_flags': 2}):
            with self.subTest(state=state), self.assertRaises((oracle.OracleError, ValueError)):
                oracle.initial_state(loaded, state)


class OutputPolicyTests(unittest.TestCase):
    def test_new_only_and_scratch_only(self):
        with tempfile.TemporaryDirectory(dir=oracle.SCRATCH) as temp:
            p = Path(temp) / 'trace.jsonl'
            self.assertEqual(oracle.output_path(p), p)
            p.write_bytes(b'keep')
            with self.assertRaises(oracle.OracleError):
                oracle.output_path(p)
            link = Path(temp) / 'link'
            link.symlink_to(Path(temp) / 'absent')
            with self.assertRaises(oracle.OracleError):
                oracle.output_path(link)
            with self.assertRaises(oracle.OracleError):
                oracle.output_path(Path(temp) / 'missing' / 'trace')
            self.assertEqual(p.read_bytes(), b'keep')
        with self.assertRaises(oracle.OracleError):
            oracle.output_path(Path.home() / 'not-an-oracle-output.json')

    def test_stop_parser_is_strict(self):
        self.assertEqual(oracle.stop_address('33c4:04C0'), (0x33c4, 0x4c0))
        for text in ('10000:0', '-1:0', 'x:3', '0x1010:0', '10:0junk'):
            with self.assertRaises(argparse.ArgumentTypeError):
                oracle.stop_address(text)


@unittest.skipUnless(HAS_CPU, 'separate dev Unicorn/Capstone environment not installed')
class CpuTests(unittest.TestCase):
    def test_bare_98_is_cbw_despite_capstone_mnemonic(self):
        _, trace, result = run_code(bytes.fromhex('b8800098'))
        self.assertEqual(result['reason'],'stop')
        self.assertEqual(result['registers']['ax'],0xff80)
        self.assertEqual(trace[-1]['mnemonic'],'cbw')
        _, _, result = run_code(bytes.fromhex('6698'))
        self.assertEqual(result['reason'],'fault')

    def test_installed_interrupt_executes_handler_and_iret(self):
        code = bytearray(40)
        code[:13] = bytes.fromhex('0e1fba2000b81225cd21cd1290')
        code[32:36] = bytes.fromhex('b83412cf')
        machine, trace, result = run_code(code, stops=[(0x1010, 13)])
        self.assertEqual(result['reason'], 'stop')
        self.assertEqual(result['registers']['ax'], 0x1234)
        self.assertEqual(result['registers']['sp'], 512)
        self.assertTrue(any(row['before']['ip'] == 32 for row in trace))
        self.assertTrue(any(row['mnemonic'] == 'iret' for row in trace))

    def test_arithmetic_stack_far_transfer_and_stop_before_execution(self):
        # mov ax,7fff; add ax,1; push ax; pop bx; call far 1010:0010;
        # mov cx,5678; nop; far target: inc bx; retf
        code = bytes.fromhex('b8ff7f 050100 50 5b 9a10001010 b93412 43 cb')
        machine, rows, state = run_code(code, stops=[(0x1010, 13)])
        self.assertEqual(state['reason'], 'stop')
        self.assertEqual([r['ip'] for r in rows], [0, 3, 6, 7, 8, 16, 17])
        self.assertEqual(state['registers']['ax'], 0x8000)
        self.assertEqual(state['registers']['bx'], 0x8001)
        self.assertEqual(state['registers']['sp'], 512)
        self.assertTrue(rows[1]['after']['flags'] & oracle.OF)
        self.assertEqual(rows[1]['defined_flags_after'], oracle.TRACKED_FLAGS)
        self.assertTrue(all(row['status'] == 'completed' for row in rows))

    def test_rep_is_one_architectural_instruction(self):
        # rep stosb; nop -- 3 stores plus Unicorn's terminal hook pass.
        machine, rows, state = run_code(bytes.fromhex('f3aa 90'),
                                       registers={'ax': 0x41, 'cx': 3, 'di': 0x200})
        self.assertEqual(state['reason'], 'stop')
        self.assertEqual(state['steps'], 2)
        self.assertEqual(len(rows), 2)
        self.assertGreater(rows[0]['microsteps'], 1)
        self.assertEqual(rows[0]['before']['cx'], 3)
        self.assertEqual(rows[0]['after']['cx'], 0)
        self.assertEqual(machine.memory()[0x10200:0x10203], b'AAA')

    def test_rep_zero_and_repe_early_termination(self):
        _, rows, state = run_code(bytes.fromhex('f3aa90'), registers={'cx': 0})
        self.assertEqual(state['steps'], 2)
        self.assertEqual(rows[0]['after']['di'], 0)
        # Equal zero at ES:0100, then unequal instruction byte at ES:0101.
        machine, rows, state = run_code(bytes.fromhex('f3ae90'),
                                       registers={'cx': 4, 'di': 0x100, 'es': 0x1010})
        self.assertEqual(state['reason'], 'stop')
        self.assertEqual(state['steps'], 2)
        # At 1010:0100 all bytes are zero, so four matches; force early case below.
        loaded = oracle.load_mz(fixture(bytes.fromhex('f3ae90')))
        loaded.registers.update(cx=4, di=0x100, es=0x1010)
        loaded.memory[0x10201] = 1
        machine = oracle.Oracle(loaded)
        rows = []
        state = machine.run(stops=[(0x1010, 2)], emit=rows.append)
        self.assertEqual(state['steps'], 1)
        self.assertEqual(state['registers']['cx'], 2)
        self.assertFalse(state['registers']['flags'] & oracle.ZF)

    def test_self_loop_budget_and_partial_rep_are_failures(self):
        _, rows, state = run_code(bytes.fromhex('ebfe'), max_instructions=3)
        self.assertEqual(state['reason'], 'instruction_limit')
        self.assertEqual(len(rows), 3)
        self.assertEqual(state['steps'], 3)
        self.assertTrue(all(row['microsteps'] == 1 for row in rows))
        _, rows, state = run_code(bytes.fromhex('f3aa'), registers={'cx': 100}, max_microsteps=2)
        self.assertEqual(state['reason'], 'microstep_limit')
        self.assertEqual(state['steps'], 0)
        self.assertEqual(rows[0]['status'], 'incomplete')
        self.assertEqual(state['registers']['cx'], 98)

    def test_defined_flags_logic_mul_shift_zero_and_one(self):
        # xor ax,ax; mov ax,2; mov bx,3; mul bx; shl ax,0; shl ax,1; shl ax,2
        _, rows, state = run_code(bytes.fromhex('31c0 b80200 bb0300 f7e3 c1e000 d1e0 c1e002'))
        self.assertEqual(state['reason'], 'stop')
        self.assertFalse(rows[0]['defined_flags_after'] & oracle.AF)
        self.assertEqual(rows[3]['defined_flags_after'] & oracle.ARITHMETIC, oracle.CF | oracle.OF)
        self.assertEqual(rows[4]['defined_flags_after'], rows[3]['defined_flags_after'])
        self.assertEqual(rows[5]['defined_flags_after'] & oracle.ARITHMETIC,
                         oracle.ARITHMETIC & ~oracle.AF)
        self.assertEqual(rows[6]['defined_flags_after'] & oracle.ARITHMETIC,
                         oracle.ARITHMETIC & ~(oracle.AF | oracle.OF))
        # Authored RCL AL,10 / RCR AX,18 reduce to one rotation, but OF
        # is undefined because the masked count is not one. Effective zero
        # (RCL AL,9 / RCR AX,17) and masked zero preserve all flag definedness.
        for code, defined in (('c0d00a', oracle.TRACKED_FLAGS & ~oracle.OF),
                              ('c1d812', oracle.TRACKED_FLAGS & ~oracle.OF),
                              ('c0d009', oracle.TRACKED_FLAGS),
                              ('c1d811', oracle.TRACKED_FLAGS),
                              ('c0d020', oracle.TRACKED_FLAGS),
                              ('d0d0', oracle.TRACKED_FLAGS)):
            with self.subTest(rotate=code):
                _, rotate, state = run_code(bytes.fromhex(code))
                self.assertEqual(state['reason'], 'stop')
                self.assertEqual(rotate[0]['defined_flags_after'], defined)

    def test_a20_alias_is_shared_not_a_stale_copy(self):
        # mov ax,ffff; mov ds,ax; mov byte [0010],5a; xor ax,ax;
        # mov ds,ax; mov bl,[0000]
        machine, _, state = run_code(bytes.fromhex('b8ffff 8ed8 c60610005a 31c0 8ed8 8a1e0000'))
        self.assertEqual(state['reason'], 'stop')
        self.assertEqual(state['registers']['bx'], 0x5a)
        self.assertEqual(machine.memory()[0], 0x5a)

    def test_dos_version_vectors_ioctl_resize_exit(self):
        # Version -> install vector 60h -> get it -> IOCTL stdin -> resize PSP -> exit 17.
        code = bytes.fromhex('b80030 cd21 b86025 ba7856 cd21 b86035 cd21 '
                             'b80044 bb0000 cd21 b80010 8ec0 bb0001 b8004a cd21 b8114c cd21')
        machine, rows, state = run_code(code)
        self.assertEqual(state['reason'], 'guest_exit')
        self.assertEqual(state['exit_code'], 17)
        self.assertEqual(machine.memory()[0x180:0x184], b'\x78\x56\x00\x10')
        self.assertEqual(rows[1]['after']['ax'], 5)
        self.assertEqual(rows[1]['after']['bx'], 0xff00)
        self.assertEqual(state['psp_block_paragraphs'], 0x100)
        self.assertFalse(state['registers']['flags'] & oracle.CF)
        self.assertEqual(len(state['services']), 6)
        self.assertEqual(rows[-1]['status'], 'completed')

    def test_bad_handle_and_resize_errors_are_explicit_carry_errors(self):
        for regs, ax, bx in (({'ax': 0x4400, 'bx': 5}, 6, 5),
                             ({'ax': 0x4a00, 'bx': 1, 'es': 0}, 9, 1),
                             ({'ax': 0x4a00, 'bx': 0}, 8, 0x8f00)):
            _, rows, state = run_code(bytes.fromhex('cd21'), registers=regs)
            self.assertEqual(state['reason'], 'stop')
            self.assertTrue(state['registers']['flags'] & oracle.CF)
            self.assertEqual(state['registers']['ax'], ax)
            self.assertEqual(state['registers']['bx'], bx)

    def test_version_query_one_and_unknown_selector(self):
        _, rows, state = run_code(bytes.fromhex('cd21'), registers={'ax': 0x3001, 'bx': 0xffff})
        self.assertEqual(state['reason'], 'stop')
        self.assertEqual(state['registers']['ax'], 5)
        self.assertEqual(state['registers']['bx'], 0)
        _, rows, state = run_code(bytes.fromhex('cd21'), registers={'ax': 0x3002})
        self.assertEqual(state['reason'], 'fault')
        self.assertEqual(state['steps'], 0)
        self.assertEqual(rows[0]['status'], 'incomplete')

    def test_unknown_services_and_all_port_forms_fail_closed(self):
        blocked = ('cd10', 'cd80', 'cd20', 'e480', 'e580', 'e680', 'e780',
                   'ec', 'ed', 'ee', 'ef', '6c', '6d', '6e', '6f', 'f36c',
                   '0f05', '0f34', '0fa2', '0f31', 'f4', '0f20c0', '660100', '648b00')
        for hex_code in blocked:
            with self.subTest(code=hex_code):
                _, _, state = run_code(bytes.fromhex(hex_code))
                self.assertEqual(state['reason'], 'fault')
                self.assertIsNone(state['exit_code'])
                self.assertEqual(state['steps'], 0)
        for ax in (0x3d00, 0x3f00, 0x4000, 0x4b00, 0x4800, 0x0900, 0xffff):
            with self.subTest(ax=ax):
                _, rows, state = run_code(bytes.fromhex('cd21'), registers={'ax': ax})
                self.assertEqual(state['reason'], 'fault')
                self.assertIn('unsupported DOS', state['fault'])
                self.assertEqual(rows[0]['status'], 'incomplete')

    def test_divide_exception_is_not_success(self):
        _, _, state = run_code(bytes.fromhex('f7f3'), registers={'bx': 0})
        self.assertEqual(state['reason'], 'fault')
        self.assertEqual(state['steps'], 0)
        self.assertIn('CPU exception 00', state['fault'])

    def test_exact_cs_ip_stop_not_linear_alias(self):
        _, rows, state = run_code(b'\x90', stops=[(0x1000, 0x100), (0x1010, 1)])
        self.assertEqual(state['steps'], 1)
        self.assertEqual(state['reason'], 'stop')
        self.assertEqual(len(rows), 1)

    def test_cli_outputs_exclusive_and_limit_nonzero(self):
        with tempfile.TemporaryDirectory(dir=oracle.SCRATCH) as temp:
            root = Path(temp)
            exe = root / 'fixture.exe'
            exe.write_bytes(fixture(bytes.fromhex('ebfe')))
            paths = [root / n for n in ('trace.jsonl', 'memory.bin', 'state.json')]
            args = ['--exe', str(exe), '--max-instructions', '3']
            for name, path in zip(('trace', 'memory', 'state'), paths):
                args.extend(['--' + name, str(path)])
            out, err = io.StringIO(), io.StringIO()
            with contextlib.redirect_stdout(out), contextlib.redirect_stderr(err):
                status = oracle.main(args)
            self.assertEqual(status, 1)
            state = json.loads(out.getvalue())
            self.assertEqual(state, json.loads(paths[2].read_text()))
            self.assertEqual(state['reason'], 'instruction_limit')
            self.assertIn('FAIL', err.getvalue())
            self.assertEqual(len(paths[0].read_text().splitlines()), 3)
            self.assertEqual(paths[1].stat().st_size, oracle.RAM_SIZE)
            original = [p.read_bytes() for p in paths]
            with contextlib.redirect_stdout(io.StringIO()), contextlib.redirect_stderr(io.StringIO()):
                self.assertEqual(oracle.main(args), 2)
            self.assertEqual(original, [p.read_bytes() for p in paths])
            # A subprocess exercises the real CLI exit status as well.
            run = subprocess.run([sys.executable, '-B', str(TOOL), '--exe', str(exe),
                                  '--max-instructions', '1'], capture_output=True, text=True)
            self.assertEqual(run.returncode, 1)
            self.assertEqual(json.loads(run.stdout)['reason'], 'instruction_limit')

    def test_cli_memory_input_leaf_and_duplicate_output_rejection(self):
        with tempfile.TemporaryDirectory(dir=oracle.SCRATCH) as temp:
            root = Path(temp)
            exe, memory, state = (root / n for n in ('fixture.exe', 'input.bin', 'input.json'))
            exe.write_bytes(fixture(b'\x90'))
            loaded = oracle.load_mz(exe.read_bytes())
            loaded.memory[0x20000:0x20004] = bytes.fromhex('b8341290')
            memory.write_bytes(loaded.memory)
            state.write_text(json.dumps({'registers': {'cs': 0x2000, 'ip': 0}}))
            args = ['--exe', str(exe), '--memory-input', str(memory), '--initial-state', str(state),
                    '--stop', '2000:0003']
            out = io.StringIO()
            with contextlib.redirect_stdout(out):
                self.assertEqual(oracle.main(args), 0)
            self.assertEqual(json.loads(out.getvalue())['registers']['ax'], 0x1234)
            duplicate = root / 'duplicate'
            with contextlib.redirect_stderr(io.StringIO()):
                self.assertEqual(oracle.main(args + ['--trace', str(duplicate), '--state', str(duplicate)]), 2)
            self.assertFalse(duplicate.exists())

    def test_self_modified_code_redecodes_and_blocks_output(self):
        # Replace the next NOP by OUT DX,AL, jump to it, fail before I/O.
        code = bytes.fromhex('c6060800ee eb01 90 90')
        loaded = oracle.load_mz(fixture(code))
        loaded.registers['ds'] = 0x1010
        machine = oracle.Oracle(loaded)
        state = machine.run(max_instructions=20)
        self.assertEqual(state['reason'], 'fault')
        self.assertIn('out', state['fault'])
        self.assertEqual(state['steps'], 2)

    def test_rep_cannot_self_modify_into_unchecked_io(self):
        # STOSB would overwrite its own REP prefix with OUT DX,AL.
        _, rows, state = run_code(bytes.fromhex('f3aa90'),
                                 registers={'ax': 0xee, 'cx': 2, 'di': 0x100})
        self.assertEqual(state['reason'], 'fault')
        self.assertEqual(state['steps'], 0)
        self.assertEqual(rows[0]['status'], 'incomplete')
        self.assertIn('self-modified REP', state['fault'])


@unittest.skipUnless(HAS_CPU and os.environ.get('PAL_ORACLE_EXE'), 'opt-in PAL_ORACLE_EXE not supplied')
class RealInputTests(unittest.TestCase):
    def test_real_startup_manifest_all_relocations_and_baseline(self):
        data = Path(os.environ['PAL_ORACLE_EXE']).read_bytes()
        manifest = json.loads((TOOL.parents[1] / 'docs' / 'resource-manifest.json').read_text())
        match = oracle.verify_manifest(data, manifest)
        self.assertEqual(match['name'], 'PAL.raw.exe')
        loaded = oracle.load_mz(data)
        self.assertEqual(loaded.metadata['relocations'], 2376)
        # Independent byte-by-byte expected relocation check over the entire table.
        expected = bytearray(data[9536:210128])
        table = int.from_bytes(data[24:26], 'little')
        for index in range(2376):
            off = int.from_bytes(data[table + index * 4:table + index * 4 + 2], 'little')
            seg = int.from_bytes(data[table + index * 4 + 2:table + index * 4 + 4], 'little')
            at = 16 * seg + off
            value = int.from_bytes(expected[at:at + 2], 'little')
            expected[at:at + 2] = ((value + 0x1010) % 65536).to_bytes(2, 'little')
        self.assertEqual(loaded.memory[0x10100:0x10100 + len(expected)], expected)
        machine = oracle.Oracle(loaded)
        rows = []
        state = machine.run(stops=[(0x33c4, 0x4c0)], emit=rows.append)
        self.assertEqual(state['reason'], 'stop')
        self.assertEqual(state['steps'], 50)
        self.assertEqual(state['microsteps'], 54)
        self.assertEqual(state['registers'], dict(ax=0, bx=0x3f48, cx=0, dx=0, si=0x4f48,
                                                di=0x1c10, bp=0, sp=0x280e, cs=0x33c4,
                                                ip=0x4c0, ds=0x3f48, es=0x3f48, ss=0x3f48,
                                                flags=0x246))
        self.assertEqual(state['memory_sha256'],
                         'cf437dba59022d6b645e5fb7d3a765c9472c60629b5a815b80f6415d9aa57894')
        self.assertEqual(len(rows), state['steps'])
        self.assertEqual([s['ax'] >> 8 for s in state['services']], [0x30, 0x4a])


if __name__ == '__main__':
    unittest.main()
