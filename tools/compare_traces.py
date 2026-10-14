#!/usr/bin/env python3
# SPDX-License-Identifier: GPL-3.0-or-later
"""Compare independent oracle instructions against AOT pre-step trace.

Native REP executes in bounded batches; only intermediate equal-CS:IP REP
states may be skipped. All other rows, every register and every architecturally
defined flag must match. Full RAM snapshots must also match byte for byte.
"""
import argparse
import hashlib
import json
from pathlib import Path


def compare(oracle, native, oracle_memory, native_memory):
    index = 0
    def state(want, got, mask, location):
        for key, value in want.items():
            if key == 'flags':
                if (value ^ got[key]) & mask:
                    raise ValueError(f'{location}: flags {value:#x} != {got[key]:#x} mask={mask:#x}')
            elif value != got[key]:
                raise ValueError(f'{location}: {key} {value:#x} != {got[key]:#x}')
    if not oracle or not native:
        raise ValueError('empty traces do not establish equivalence')
    for row in oracle:
        if row['status'] != 'completed':
            raise ValueError('oracle contains an incomplete instruction')
        location = f"{row['cs']:04x}:{row['ip']:04x} step={row['step']}"
        if index >= len(native):
            raise ValueError('native trace ended prematurely')
        state(row['before'], native[index], row['defined_flags_before'], location+' before')
        index += 1
        if row['mnemonic'].startswith(('rep ', 'repe ', 'repne ')):
            while index < len(native)-1 and (native[index]['cs'], native[index]['ip']) == (row['cs'], row['ip']):
                index += 1
        if index >= len(native):
            raise ValueError('native final state is missing')
        state(row['after'], native[index], row['defined_flags_after'], location+' after')
    if index != len(native)-1:
        raise ValueError('unmatched native trace suffix')
    if len(oracle_memory) != 1 << 20 or len(native_memory) != 1 << 20:
        raise ValueError('expected two complete 1-MiB physical RAM snapshots')
    if oracle_memory != native_memory:
        first = next(i for i, (a,b) in enumerate(zip(oracle_memory,native_memory)) if a != b)
        raise ValueError(f'memory mismatch first physical address {first:#x}')
    return dict(architectural_instructions=len(oracle), native_states=len(native),
                memory_bytes=len(oracle_memory), memory_sha256=hashlib.sha256(oracle_memory).hexdigest())


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    for name in ('oracle-trace','native-trace','oracle-memory','native-memory'):
        parser.add_argument('--'+name, type=Path, required=True)
    args = parser.parse_args()
    try:
        rows = lambda path: [json.loads(line) for line in path.read_text().splitlines() if line]
        result = compare(rows(args.oracle_trace), rows(args.native_trace),
                         args.oracle_memory.read_bytes(), args.native_memory.read_bytes())
    except (OSError, ValueError, KeyError, TypeError) as error:
        parser.exit(1, f'differential: {error}\n')
    print(json.dumps(dict(status='matched', **result), indent=2))


if __name__ == '__main__':
    main()
