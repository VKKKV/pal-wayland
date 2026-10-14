# SPDX-License-Identifier: GPL-3.0-or-later
import importlib.util
from pathlib import Path
from typing import Any
import unittest

spec = importlib.util.spec_from_file_location('compare', Path(__file__).resolve().parents[1]/'tools/compare_traces.py')
assert spec is not None and spec.loader is not None
compare = importlib.util.module_from_spec(spec)
spec.loader.exec_module(compare)

class ComparisonTests(unittest.TestCase):
    def fixture(self) -> tuple[list[dict[str, Any]], list[dict[str, int]], bytes, bytes]:
        before = dict(cs=0x1000, ip=0x100, ax=1, flags=0x202)
        after = dict(before, ip=0x102, ax=2, flags=0x203)
        row = dict(step=0, cs=before['cs'], ip=before['ip'], mnemonic='add', status='completed',
                   before=before, after=after, defined_flags_before=0xfff, defined_flags_after=0xfff)
        return [row], [before,after], bytes(1<<20),bytes(1<<20)
    def test_full_match_and_undefined_flags(self):
        data = self.fixture()
        self.assertEqual(compare.compare(*data)['architectural_instructions'],1)
        data[0][0]['defined_flags_after'] &= ~1
        data[1][1] = dict(data[1][1], flags=0x202)
        compare.compare(*data)
    def test_register_flags_memory_and_suffix_fail(self):
        for kind in ['register','flags','memory','suffix','status','empty','short']:
            data: list[Any] = list(self.fixture())
            if kind == 'register': data[1][1] = dict(data[1][1],ax=3)
            if kind == 'flags': data[1][1] = dict(data[1][1],flags=0x202)
            if kind == 'memory': data[3] = bytes([1])+data[3][1:]
            if kind == 'suffix': data[1].append(dict(data[1][-1]))
            if kind == 'status': data[0][0]['status']='interrupted'
            if kind == 'empty': data[0]=[]
            if kind == 'short': data[1].pop()
            with self.subTest(kind=kind), self.assertRaises(ValueError): compare.compare(*data)
    def test_rep_intermediate_states_only(self):
        data = self.fixture()
        data[0][0]['mnemonic']='rep movsb'
        data[1].insert(1,dict(data[1][0],ax=99))
        compare.compare(*data)
        data[0][0]['mnemonic']='add'
        with self.assertRaises(ValueError): compare.compare(*data)

if __name__=='__main__':unittest.main()
