#!/usr/bin/env python3
# SPDX-License-Identifier: GPL-3.0-or-later
"""Development-only independent Unicorn oracle; NEVER a native runtime backend.

Requires Unicorn 2.1.4 and Capstone 5.0.7 in a separate development environment.
No DOS/BIOS image, asset mount, filesystem service, network, audio or GUI exists.
Only INT 21h services 25h/30h/35h/4400h/4Ah/4Ch are implemented; all port I/O,
other interrupts, and instructions outside the integer real-mode allowlist FAIL.

Example (all output parents must already exist):
  python tools/oracle_trace.py --exe assets/unpacked/PAL.raw.exe \
    --manifest docs/resource-manifest.json --stop 33c4:04c0 \
    --max-instructions 100000 --trace "$TMPDIR/startup.jsonl" \
    --memory "$TMPDIR/startup.bin" --state "$TMPDIR/startup.json"

Output paths must be beneath $TMPDIR (when set) or this
repository's generated/ directory. Exclusive creation: existing files/symlinks
are never replaced. Output filenames come only from CLI, never from guest data.
The memory output is exactly 1 MiB, physical address zero first, A20 disabled.

JSONL schema pal-oracle-trace-v1: one row per *architectural* instruction:
  step (zero based), cs/ip (pre-execution, integers), bytes (hex), mnemonic,
  op_str, before/after (lower-case 16-bit registers incl flags),
  defined_flags_before/after (comparison bit masks), microsteps, status.
Compare FLAGS using (oracle ^ native) & defined_flags_after, not reserved or
architecturally undefined bits. This is flag-definedness, NOT full data taint.
REP hook iterations are collapsed into one row (including Unicorn's final
zero-count hook); max-microsteps separately bounds this work. A failed/interrupted
row is explicitly non-completed and is NOT a valid differential expectation.
Stop addresses are loaded, absolute hexadecimal CS:IP and stop BEFORE execution.
State JSON contains reason, fault, registers, masks, step counts, service events,
input/loader provenance and memory SHA-256. CLI also prints that JSON to stdout;
errors go to stderr. Exit 0 = stop/guest_exit, 1 = execution failure/limit/timeout,
2 = setup/arguments/output failure. Guest exit_code is recorded, not propagated.

--initial-state accepts a JSON object {"registers": {"cs": 13252, "ip": "0x4c0",
"ax": 0, ...}, "defined_flags": 4053}. Omitted registers retain loader defaults.
--memory-input replaces ALL loaded physical RAM with an exact 1-MiB snapshot;
it requires --initial-state. These inputs seed leaf experiments, not a general
DOS checkpoint: the sole PSP allocation always starts at its loader default.

DOS handlers intentionally match the selected Machine test environment, not an
independent DOS implementation. CPU agreement through INT 21h validates only the
chosen shim contract; it is not evidence of full historical DOS compatibility.
In particular unsuccessful 4Ah allocation side effects are outside this scope.
"""
from __future__ import annotations

import argparse
from contextlib import ExitStack
import ctypes
from dataclasses import dataclass
import hashlib
import json
import os
from pathlib import Path
import re
import struct
import sys
from typing import Callable

RAM_SIZE = 1 << 20
TOP = 0x9F00
ENV = 0x0800
CF, PF, AF, ZF, SF, TF, IF, DF, OF = (1 << n for n in (0, 2, 4, 6, 7, 8, 9, 10, 11))
ARITHMETIC = CF | PF | AF | ZF | SF | OF
TRACKED_FLAGS = ARITHMETIC | TF | IF | DF
REGISTER_NAMES = ('ax', 'bx', 'cx', 'dx', 'si', 'di', 'bp', 'sp',
                  'cs', 'ip', 'ds', 'es', 'ss', 'flags')
REPO = Path(__file__).resolve().parents[1]
SCRATCH = Path(os.environ.get('TMPDIR', str(REPO / 'generated'))).resolve()


class OracleError(ValueError):
    """A closed-boundary failure, not successful guest execution."""


@dataclass
class LoadedMz:
    memory: bytearray
    registers: dict[str, int]
    metadata: dict


def load_mz(data: bytes, load_segment: int = 0x1010) -> LoadedMz:
    """Independent MZ parser/relocator; no Rust/native loader is invoked."""
    if not 0x1010 <= load_segment < TOP:
        raise OracleError('load segment must be >= 0x1010 and below 0x9f00')
    if len(data) < 28 or data[:2] not in (b'MZ', b'ZM'):
        raise OracleError('not a DOS MZ executable')
    (magic, last, pages, count, paragraphs, minimum, maximum, ss, sp, checksum,
     ip, cs, table, overlay) = struct.unpack_from('<14H', data)
    if pages == 0 or last >= 512:
        raise OracleError('invalid MZ page count/last-page size')
    declared = (pages - 1) * 512 + (last or 512)
    header_size = paragraphs * 16
    if not 28 <= header_size <= declared <= len(data):
        raise OracleError('invalid/truncated MZ header or declared size')
    if minimum > maximum or overlay:
        raise OracleError('invalid extra allocation or unsupported overlay')
    if header_size >= 64 and (table == 0x40 or (table == 0 and count == 0)):
        at = struct.unpack_from('<I', data, 0x3c)[0]
        if at >= 64 and (data[at:at + 2] in (b'NE', b'LE', b'LX')
                         or data[at:at + 4] == b'PE\0\0'):
            raise OracleError('unsupported secondary executable header')
    if count and not (28 <= table and table + count * 4 <= header_size):
        raise OracleError('relocation table outside MZ header')
    image = bytearray(data[header_size:declared])
    image_size = len(image)
    allocation = ((image_size + 15) // 16 + minimum) * 16
    base = load_segment * 16
    if base + allocation > TOP * 16:
        raise OracleError('image allocation exceeds conventional RAM')
    entry = (cs * 16 + ip) % RAM_SIZE
    loaded_cs, loaded_ss = (cs + load_segment) & 0xffff, (ss + load_segment) & 0xffff
    if entry >= image_size or loaded_cs * 16 + ip != base + entry:
        raise OracleError('entry outside image or wrapped loaded entry')
    stack_displacement = sp or 0x10000
    stack = ss * 16 + stack_displacement
    if sp == 1 or stack > allocation or loaded_ss * 16 + stack_displacement != base + stack:
        raise OracleError('stack outside allocation or wrapped stack')
    for index in range(count):
        off, seg = struct.unpack_from('<HH', data, table + index * 4)
        target = seg * 16 + off
        if target + 2 > image_size:
            raise OracleError(f'relocation {index} outside unpadded image')
        value = (struct.unpack_from('<H', image, target)[0] + load_segment) & 0xffff
        struct.pack_into('<H', image, target, value)
    memory = bytearray(RAM_SIZE)
    memory[base:base + image_size] = image
    psp = load_segment - 16
    struct.pack_into('<HH', memory, psp * 16, 0x20cd, TOP)
    struct.pack_into('<H', memory, psp * 16 + 0x2c, ENV)
    memory[psp * 16 + 0x81] = 13
    environment = b'PATH=C:\\\0\0\x01\0C:\\PAL.EXE\0'
    memory[ENV * 16:ENV * 16 + len(environment)] = environment
    struct.pack_into('<H', memory, 0x413, TOP >> 6)
    registers: dict[str, int] = dict.fromkeys(REGISTER_NAMES, 0)
    registers.update(cs=loaded_cs, ip=ip, ss=loaded_ss, sp=sp, ds=psp, es=psp, flags=0x202)
    metadata = dict(exe_sha256=hashlib.sha256(data).hexdigest(), exe_bytes=len(data),
                    declared_bytes=declared, header_bytes=header_size, image_bytes=image_size,
                    allocation_bytes=allocation, relocations=count, load_segment=load_segment,
                    psp=psp, conventional_top=TOP, environment_segment=ENV, dos_version=5,
                    a20='disabled', initial_memory_sha256=hashlib.sha256(memory).hexdigest())
    return LoadedMz(memory, registers, metadata)


def verify_manifest(data: bytes, manifest: dict) -> dict:
    digest = hashlib.sha256(data).hexdigest()
    for entry in manifest.get('files', []) + manifest.get('unpacked_files', []):
        if entry.get('sha256') == digest and entry.get('size') == len(data):
            return dict(name=entry['name'], size=entry['size'], sha256=digest)
    raise OracleError('input size/SHA-256 does not match any manifest entry')


def word_value(value) -> int:
    if isinstance(value, str):
        value = int(value, 0)
    if type(value) is not int or not 0 <= value <= 0xffff:
        raise OracleError('register values must be integers in 0..65535')
    return value


def initial_state(loaded: LoadedMz, value: dict) -> int:
    if not isinstance(value, dict) or set(value) - {'registers', 'defined_flags'}:
        raise OracleError('initial state keys must be registers/defined_flags')
    registers = value.get('registers', {})
    if not isinstance(registers, dict) or set(registers) - set(REGISTER_NAMES):
        raise OracleError('unknown initial-state register')
    parsed = {name: word_value(v) for name, v in registers.items()}
    # IF is just a bit, no asynchronous devices. TF would deliver debug traps.
    if parsed.get('flags', loaded.registers['flags']) & ~TRACKED_FLAGS & ~2:
        raise OracleError('unsupported initial FLAGS bits')
    if parsed.get('flags', loaded.registers['flags']) & TF:
        raise OracleError('initial trap flag is unsupported')
    defined = word_value(value.get('defined_flags', TRACKED_FLAGS))
    if defined & ~TRACKED_FLAGS:
        raise OracleError('defined_flags contains reserved flags')
    loaded.registers.update(parsed)
    return defined


# Small explicit integer-CPU scope. The allowlist protects against privileged,
# nondeterministic, system, vector/FPU, 32-bit and port instructions even when
# Unicorn would otherwise silently emulate one of them.
ALLOWED = set('''mov lea xchg push pop pushf popf pusha popa cbw cwd
add adc sub sbb cmp inc dec neg not and or xor test mul imul div idiv
shl sal shr sar rol ror rcl rcr clc stc cmc cld std cli sti lahf sahf
call lcall jmp ljmp ret retf iret nop int into
ja jae jb jbe jc jcxz je jg jge jl jle jna jnae jnb jnbe jnc jne jng
jnge jnl jnle jno jnp jns jnz jo jp jpe jpo js jz loop loope loopne
les lds enter leave movsb movsw lodsb lodsw stosb stosw cmpsb cmpsw
scasb scasw xlatb aaa aas aad aam daa das'''.split())
STRINGS = {'movsb', 'movsw', 'lodsb', 'lodsw', 'stosb', 'stosw',
           'cmpsb', 'cmpsw', 'scasb', 'scasw'}
SHIFTS = {'shl', 'sal', 'shr', 'sar', 'rol', 'ror', 'rcl', 'rcr'}


class Oracle:
    def __init__(self, loaded: LoadedMz, *, defined_flags: int = TRACKED_FLAGS):
        # Lazy imports permit loader and safety-policy tests without dependencies.
        import capstone
        from capstone import x86_const
        import unicorn
        from unicorn import x86_const as uc_x86
        self.unicorn, self.capstone, self.x86 = unicorn, capstone, x86_const
        self.uc = unicorn.Uc(unicorn.UC_ARCH_X86, unicorn.UC_MODE_16)
        self.decoder = capstone.Cs(capstone.CS_ARCH_X86, capstone.CS_MODE_16)
        self.decoder.detail = True
        self.reg_ids = {n: getattr(uc_x86, 'UC_X86_REG_' + ('EFLAGS' if n == 'flags' else n.upper()))
                        for n in REGISTER_NAMES}
        # Same backing storage for the HMA alias: A20 clears bit 20 per byte.
        self.backing = ctypes.create_string_buffer(bytes(loaded.memory), RAM_SIZE)
        self.uc.mem_map_ptr(0, RAM_SIZE, unicorn.UC_PROT_ALL, ctypes.addressof(self.backing))
        self.uc.mem_map_ptr(RAM_SIZE, 0x10000, unicorn.UC_PROT_ALL, ctypes.addressof(self.backing))
        for name, value in loaded.registers.items():
            self.set_reg(name, value)
        self.defined_flags = defined_flags
        self.metadata = dict(loaded.metadata)
        self.metadata.update(unicorn_version=unicorn.__version__, capstone_version=capstone.__version__)
        self.psp = loaded.metadata['psp']
        self.block_size = TOP - self.psp
        self.steps = self.microsteps = 0
        self.exit_code = None
        self.reason = self.fault = None
        self.pending: tuple[dict, object, str, bool] | None = None
        self.services = []
        self.uc.hook_add(unicorn.UC_HOOK_CODE, self._code)
        self.uc.hook_add(unicorn.UC_HOOK_INTR, self._interrupt)
        self.uc.hook_add(unicorn.UC_HOOK_MEM_INVALID, self._invalid_memory)
        self._cache = {}
        self._ran = False

    def reg(self, name: str) -> int:
        return self.uc.reg_read(self.reg_ids[name]) & 0xffff

    def set_reg(self, name: str, value: int):
        self.uc.reg_write(self.reg_ids[name], value & 0xffff)

    def registers(self) -> dict:
        return {name: self.reg(name) for name in REGISTER_NAMES}

    def memory(self) -> bytes:
        return bytes(self.uc.mem_read(0, RAM_SIZE))

    def _halt(self, reason, message=None):
        if self.reason is None:
            self.reason, self.fault = reason, message
        self.uc.emu_stop()

    def _invalid_memory(self, uc, access, address, size, value, user_data):
        self._halt('fault', f'unmapped memory access type={access} address={address:#x} size={size}')
        return False

    def _decode(self, cs, ip, size):
        if not 1 <= size <= 15 or ip + size > 0x10000:
            raise OracleError('invalid instruction length or cross-segment fetch')
        raw = bytes(self.uc.mem_read(cs * 16 + ip, size))
        # Cache by bytes, never just address: modified code must be re-decoded.
        key = (raw, ip)
        if key not in self._cache:
            instruction = next(self.decoder.disasm(raw, ip, count=1), None)
            if instruction is None or instruction.size != size:
                raise OracleError(f'cannot decode instruction {raw.hex()}')
            self._cache[key] = instruction
        ins = self._cache[key]
        mnemonic = ins.mnemonic.split()[-1]
        # Capstone 5.0.7 calls bare 98h 'cwde' even in 16-bit mode.
        # Normalize this confirmed encoding only; 66h remains unsupported.
        if raw == b'\x98':
            mnemonic = 'cbw'
        if mnemonic not in ALLOWED or 0x66 in ins.prefix or 0x67 in ins.prefix:
            raise OracleError(f'unsupported instruction {ins.mnemonic} {ins.op_str}')
        if 0xf0 in ins.prefix:
            raise OracleError('LOCK prefix unsupported')
        allowed_registers = set(REGISTER_NAMES) | {'al', 'ah', 'bl', 'bh', 'cl', 'ch', 'dl', 'dh'}
        for operand in ins.operands:
            used = ([operand.reg] if operand.type == self.x86.X86_OP_REG else
                    [operand.mem.segment, operand.mem.base, operand.mem.index]
                    if operand.type == self.x86.X86_OP_MEM else [])
            if any(reg and ins.reg_name(reg) not in allowed_registers for reg in used):
                raise OracleError('register outside the 16-bit integer state schema')
        if any(prefix in (0x64, 0x65) for prefix in ins.prefix):
            raise OracleError('FS/GS override unsupported')
        repeated = any(p in (0xf2, 0xf3) for p in ins.prefix)
        if repeated and mnemonic not in STRINGS:
            raise OracleError('REP prefix on a non-string instruction unsupported')
        return ins, raw, mnemonic, repeated

    def _flags_after(self, ins, mnemonic, before, repeated):
        mask = self.defined_flags
        if repeated and before['cx'] == 0:
            return mask
        if mnemonic in SHIFTS:
            op = ins.operands[-1]
            masked_count = (op.imm if op.type == self.x86.X86_OP_IMM else before['cx'] & 0xff) & 31
            count = masked_count
            width = ins.operands[0].size * 8
            if mnemonic in ('rcl', 'rcr'):
                count %= width + 1
            if count == 0:
                return mask
            if mnemonic in ('rol', 'ror', 'rcl', 'rcr'):
                mask = (mask | CF) & ~OF
                # Intel SDM Vol. 2B, RCL/RCR/ROL/ROR: zero-bit RCL/RCR
                # preserves flags (above); otherwise OF uses COUNT & 1FH,
                # not the modulo-9/17 rotation count.
                if masked_count == 1:
                    mask |= OF
                if mnemonic in ('rcl', 'rcr') and not self.defined_flags & CF:
                    mask &= ~(CF | OF)
                return mask
            mask = (mask | SF | ZF | PF | CF) & ~(AF | OF)
            if count == 1:
                mask |= OF
            if count >= width and mnemonic != 'sar':
                mask &= ~CF
            return mask
        for name, bit in (('CF', CF), ('PF', PF), ('AF', AF), ('ZF', ZF),
                          ('SF', SF), ('OF', OF), ('TF', TF), ('IF', IF), ('DF', DF)):
            undefined = getattr(self.x86, 'X86_EFLAGS_UNDEFINED_' + name, 0)
            writes = sum(getattr(self.x86, 'X86_EFLAGS_' + action + '_' + name, 0)
                         for action in ('MODIFY', 'RESET', 'SET'))
            if ins.eflags & writes:
                mask |= bit
            if ins.eflags & undefined:
                mask &= ~bit
        if mnemonic in ('adc', 'sbb') and not self.defined_flags & CF:
            mask &= ~ARITHMETIC
        if mnemonic == 'cmc' and not self.defined_flags & CF:
            mask &= ~CF
        return mask

    def _finish(self, status='completed'):
        if self.pending is None:
            return
        row, ins, mnemonic, repeated = self.pending
        if status == 'completed':
            self.defined_flags = self._flags_after(ins, mnemonic, row['before'], repeated)
            if row.pop('_service_sets_cf', False):
                self.defined_flags |= CF
            self.steps += 1
        else:
            row.pop('_service_sets_cf', None)
        row.update(after=self.registers(), defined_flags_after=self.defined_flags, status=status)
        if status != 'completed':
            row['fault'] = self.fault
        self.emit(row)
        self.pending = None

    def _code(self, uc, address, size, user_data):
        cs, ip = self.reg('cs'), self.reg('ip')
        # Unicorn calls UC_HOOK_CODE for each REP iteration and its terminal
        # zero-count pass. Only REP-string rows may coalesce equal addresses;
        # an ordinary JMP $ is many instructions and remains budgeted.
        continuation = (self.pending is not None and self.pending[3]
                        and (cs, ip) == (self.pending[0]['cs'], self.pending[0]['ip']))
        if continuation:
            assert self.pending is not None
            original = self.pending[0]['bytes']
            current = bytes(self.uc.mem_read(cs * 16 + ip, len(original) // 2)).hex()
            if current != original:
                self._halt('fault', 'self-modified REP instruction is unsupported')
                return
        if not continuation:
            self._finish()
            if (cs, ip) in self.stops:
                self._halt('stop')
                return
            if self.steps >= self.max_instructions:
                self._halt('instruction_limit', 'architectural instruction budget exhausted')
                return
        if self.microsteps >= self.max_microsteps:
            self._halt('microstep_limit', 'Unicorn hook/microstep budget exhausted')
            return
        if continuation:
            assert self.pending is not None
            self.microsteps += 1
            self.pending[0]['microsteps'] += 1
            return
        try:
            ins, raw, mnemonic, repeated = self._decode(cs, ip, size)
        except OracleError as exc:
            self._halt('fault', f'{cs:04x}:{ip:04x}: {exc}')
            return
        self.microsteps += 1
        self.pending = (dict(schema='pal-oracle-trace-v1', step=self.steps, cs=cs, ip=ip,
                             bytes=raw.hex(), mnemonic=('cbw' if raw == b'\x98' else ins.mnemonic), op_str=ins.op_str,
                             before=self.registers(), defined_flags_before=self.defined_flags,
                             microsteps=1), ins, mnemonic, repeated)

    def _interrupt(self, uc, number, user_data):
        # Exceptions are distinct from explicit INT. Installed vectors execute
        # real guest handler code with an architectural interrupt frame.
        location = (f"{self.pending[0]['cs']:04x}:{self.pending[0]['ip']:04x}"
                    if self.pending is not None else f"{self.reg('cs'):04x}:{self.reg('ip'):04x}")
        if self.pending is None or self.pending[2] != 'int':
            self._halt('fault', f'{location}: unsupported interrupt/CPU exception {number:02x}')
            return
        target_ip, target_cs = struct.unpack('<HH', self.uc.mem_read(number * 4, 4))
        if target_ip or target_cs:
            sp, ss = self.reg('sp'), self.reg('ss')
            flags = (self.reg('flags') & 0x7fd5) | 2
            frame = []
            for word in (flags, self.reg('cs'), self.reg('ip')):
                sp = (sp - 2) & 0xffff
                if sp == 0xffff:
                    self._halt('fault', 'interrupt stack word crosses segment')
                    return
                frame.append(((ss * 16 + sp) & 0xfffff, struct.pack('<H', word)))
            for linear, word in frame:
                self.uc.mem_write(linear, word)
            self.set_reg('sp', sp)
            self.set_reg('flags', flags & ~(IF | TF))
            self.set_reg('cs', target_cs)
            self.set_reg('ip', target_ip)
            return
        if number != 0x21:
            self._halt('fault', f'{location}: unsupported interrupt {number:02x}')
            return
        ax, bx, es = self.reg('ax'), self.reg('bx'), self.reg('es')
        self.services.append(dict(step=self.steps, interrupt=number, ax=ax, bx=bx,
                                  cx=self.reg('cx'), dx=self.reg('dx'),
                                  cs=self.pending[0]['cs'], ip=self.pending[0]['ip']))
        ah, al = ax >> 8, ax & 0xff
        if ah == 0x30:
            if al not in (0, 1):
                self._halt('fault', f'{location}: unsupported DOS version query AL={al:02x}')
                return
            self.set_reg('ax', 5)
            self.set_reg('bx', 0xff00 if al == 0 else 0)
            self.set_reg('cx', 0)
        elif ah == 0x25:
            self.uc.mem_write(al * 4, struct.pack('<HH', self.reg('dx'), self.reg('ds')))
        elif ah == 0x35:
            offset, segment = struct.unpack('<HH', self.uc.mem_read(al * 4, 4))
            self.set_reg('bx', offset)
            self.set_reg('es', segment)
        elif ah == 0x4a:
            if es != self.psp:
                self._dos_result(9)
            elif 0 < bx <= TOP - self.psp:
                self.block_size = bx
                self._dos_result()
            else:
                self.set_reg('bx', TOP - self.psp)
                self._dos_result(8)
        elif ax == 0x4400:
            if bx < 5:
                self.set_reg('dx', 0x80d3)
                self._dos_result()
            else:
                self._dos_result(6)
        elif ah == 0x4c:
            self.exit_code = al
            self._halt('guest_exit')
        else:
            self._halt('fault', f'{location}: unsupported DOS service AX={ax:04x}')

    def _dos_result(self, error=None):
        assert self.pending is not None
        flags = self.reg('flags')
        self.set_reg('flags', flags & ~CF if error is None else flags | CF)
        if error is not None:
            self.set_reg('ax', error)
        self.pending[0]['_service_sets_cf'] = True

    def run(self, *, stops=(), max_instructions=100000, max_microsteps=1000000,
            timeout_us=10000000, emit: Callable[[dict], None] | None = None) -> dict:
        if self._ran:
            raise OracleError('Oracle instances are single-run; create a fresh oracle')
        if min(max_instructions, max_microsteps, timeout_us) <= 0:
            raise OracleError('instruction, microstep and timeout budgets must be positive')
        self._ran = True
        self.stops = set(stops)
        self.max_instructions, self.max_microsteps = max_instructions, max_microsteps
        self.emit = emit or (lambda row: None)
        begin = self.reg('cs') * 16 + self.reg('ip')
        engine_error = None
        try:
            # Official unicorn.h specifies MICROSECONDS (not milliseconds).
            # until is outside all mapped RAM; hook checks exact CS:IP stops.
            self.uc.emu_start(begin, 0x110000, timeout=timeout_us, count=max_microsteps + 1)
        except self.unicorn.UcError as exc:
            engine_error = str(exc)
            self._halt('fault', 'Unicorn: ' + engine_error)
        if self.reason is None:
            if self.uc.query(self.unicorn.UC_QUERY_TIMEOUT):
                self.reason, self.fault = 'timeout', 'Unicorn wall-clock timeout exhausted'
            else:
                self.reason, self.fault = 'fault', 'Unicorn stopped without an explicit oracle boundary'
        if self.pending is not None:
            self._finish('completed' if self.reason == 'guest_exit' else 'incomplete')
        memory = self.memory()
        return dict(schema='pal-oracle-state-v1', reason=self.reason, fault=self.fault,
                    engine_error=engine_error, registers=self.registers(),
                    defined_flags=self.defined_flags, steps=self.steps, microsteps=self.microsteps,
                    exit_code=self.exit_code, services=self.services, metadata=self.metadata,
                    psp_block_paragraphs=self.block_size, memory_bytes=len(memory),
                    memory_sha256=hashlib.sha256(memory).hexdigest(),
                    limits=dict(max_instructions=max_instructions, max_microsteps=max_microsteps,
                                timeout_us=timeout_us),
                    scope='development-only independent CPU oracle; not native execution')


def stop_address(text):
    if not re.fullmatch(r'[0-9a-fA-F]{1,4}:[0-9a-fA-F]{1,4}', text):
        raise argparse.ArgumentTypeError('expected hexadecimal CS:IP (1..4 digits each)')
    return tuple(int(v, 16) for v in text.split(':'))


def positive(text):
    value = int(text, 0)
    if value <= 0:
        raise argparse.ArgumentTypeError('value must be positive')
    return value


def output_path(path: Path) -> Path:
    resolved = path.resolve()
    roots = (SCRATCH, (REPO / 'generated').resolve())
    if not any(resolved.is_relative_to(root) and resolved != root for root in roots):
        raise OracleError('outputs must be beneath scratch or repository generated/')
    if path.is_symlink() or path.exists():
        raise OracleError(f'output already exists: {path}')
    if not path.parent.is_dir():
        raise OracleError(f'output parent must already exist: {path.parent}')
    return path


def main(argv=None) -> int:
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument('--exe', required=True, type=Path)
    parser.add_argument('--load-segment', type=lambda v: int(v, 0), default=0x1010)
    parser.add_argument('--stop', action='append', type=stop_address, default=[])
    parser.add_argument('--max-instructions', type=positive, default=100000)
    parser.add_argument('--max-microsteps', type=positive, default=1000000)
    parser.add_argument('--timeout-us', type=positive, default=10000000)
    parser.add_argument('--manifest', type=Path, help='require exact size/SHA-256 match in this manifest')
    parser.add_argument('--initial-state', type=Path)
    parser.add_argument('--memory-input', type=Path)
    for option in ('trace', 'memory', 'state'):
        parser.add_argument('--' + option, type=Path)
    args = parser.parse_args(argv)
    try:
        data = args.exe.read_bytes()
        loaded = load_mz(data, args.load_segment)
        loaded.metadata['manifest_match'] = (verify_manifest(data, json.loads(args.manifest.read_text()))
                                             if args.manifest else None)
        defined = TRACKED_FLAGS
        if args.initial_state:
            state_bytes = args.initial_state.read_bytes()
            defined = initial_state(loaded, json.loads(state_bytes))
            loaded.metadata['initial_state_sha256'] = hashlib.sha256(state_bytes).hexdigest()
        if args.memory_input:
            if not args.initial_state:
                raise OracleError('--memory-input requires --initial-state')
            memory = args.memory_input.read_bytes()
            if len(memory) != RAM_SIZE:
                raise OracleError('--memory-input must be exactly 1 MiB physical RAM')
            loaded.memory[:] = memory
            loaded.metadata['memory_input_sha256'] = hashlib.sha256(memory).hexdigest()
            loaded.metadata['initial_memory_sha256'] = hashlib.sha256(memory).hexdigest()
        targets = {name: output_path(path) for name in ('trace', 'memory', 'state')
                   if (path := getattr(args, name)) is not None}
        if len({p.resolve() for p in targets.values()}) != len(targets):
            raise OracleError('output paths must be distinct')
        machine = Oracle(loaded, defined_flags=defined)
        with ExitStack() as stack:
            outputs = {name: stack.enter_context(path.open('xb')) for name, path in targets.items()}
            def emit(row):
                if 'trace' in outputs:
                    outputs['trace'].write((json.dumps(row, separators=(',', ':')) + '\n').encode())
            result = machine.run(stops=args.stop, max_instructions=args.max_instructions,
                                 max_microsteps=args.max_microsteps, timeout_us=args.timeout_us, emit=emit)
            encoded = json.dumps(result, indent=2) + '\n'
            if 'memory' in outputs:
                outputs['memory'].write(machine.memory())
            if 'state' in outputs:
                outputs['state'].write(encoded.encode())
            print(encoded, end='')
            if result['reason'] not in ('stop', 'guest_exit'):
                print('oracle FAIL: ' + (result['fault'] or result['reason']), file=sys.stderr)
                return 1
        return 0
    except (OracleError, OSError, ValueError, ImportError) as exc:
        print(f'oracle setup FAIL: {exc}', file=sys.stderr)
        return 2


if __name__ == '__main__':
    sys.exit(main())
