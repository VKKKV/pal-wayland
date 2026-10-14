# SPDX-License-Identifier: GPL-3.0-or-later
import importlib.util
import sys
import unittest
import contextlib
import ctypes
import io
import json
import os
import shutil
import subprocess
import tempfile
import signal
import time
from types import SimpleNamespace
from unittest import mock
from pathlib import Path
spec=importlib.util.spec_from_file_location("dev_native",Path(__file__).resolve().parents[1]/"tools/dev_native.py")
assert spec and spec.loader
module=importlib.util.module_from_spec(spec);spec.loader.exec_module(module)
class ProcessTests(unittest.TestCase):
    def test_success_failure_and_timeout_are_real(self):
        result=module.invoke([sys.executable,"-c","print('original probe')"],Path.cwd(),10)
        self.assertEqual(result['exit_code'],0);self.assertEqual(result['stdout'].strip(),'original probe');self.assertFalse(result['timed_out'])
        result=module.invoke([sys.executable,"-c","raise SystemExit(7)"],Path.cwd(),10);self.assertEqual(result['exit_code'],7)
        result=module.invoke([sys.executable,"-c","import time;time.sleep(10)"],Path.cwd(),0.05);self.assertEqual(result['exit_code'],124);self.assertTrue(result['timed_out'])
        # Adopt/reap only authored orphan fixtures, restoring Linux's setting.
        libc = ctypes.CDLL(None, use_errno=True)
        prior = ctypes.c_int()
        self.assertEqual(libc.prctl(37, ctypes.byref(prior), 0, 0, 0), 0)
        self.assertEqual(libc.prctl(36, 1, 0, 0, 0), 0)
        child = None
        control = None
        # Child redirects both streams, ignores TERM, and self-exits after 2s
        # even if cleanup regresses. The pipe handshake precedes the timeout.
        source = '''import os, signal, time
r,w=os.pipe()
child=os.fork()
if child == 0:
    os.close(r)
    signal.signal(signal.SIGTERM,signal.SIG_IGN)
    fd=os.open(os.devnull,os.O_WRONLY)
    os.dup2(fd,1);os.dup2(fd,2);os.close(fd)
    os.write(w,b'R');os.close(w)
    time.sleep(2);os._exit(0)
os.close(w);os.read(r,1);os.close(r)
print(child,flush=True)
time.sleep(5)
'''
        try:
            control = subprocess.Popen([sys.executable, '-c', 'import time;time.sleep(5)'],
                                       start_new_session=True, stdout=subprocess.DEVNULL,
                                       stderr=subprocess.DEVNULL)
            started = time.monotonic()
            result = module.invoke([sys.executable, '-c', source], Path.cwd(), 0.2)
            child = int(result['stdout'].strip())
            self.assertTrue(result['timed_out']); self.assertEqual(result['exit_code'], 124)
            self.assertLess(time.monotonic() - started, 1.5)
            deadline = time.monotonic() + 0.5
            while True:
                reaped, status = os.waitpid(child, os.WNOHANG)
                if reaped or time.monotonic() >= deadline:
                    break
                time.sleep(0.01)
            self.assertEqual(reaped, child, 'TERM-resistant descendant survived cleanup')
            child = None
            self.assertTrue(os.WIFSIGNALED(status))
            self.assertEqual(os.WTERMSIG(status), signal.SIGKILL)
            self.assertIsNone(control.poll(), 'unrelated process group was signalled')
        finally:
            if child is not None:
                try:
                    # Prove it is still our adopted child before signalling.
                    if os.waitpid(child, os.WNOHANG)[0] == 0:
                        os.kill(child, signal.SIGKILL); os.waitpid(child, 0)
                except ChildProcessError:
                    pass
            try:
                if control is not None:
                    control.terminate(); control.wait(timeout=1)
            finally:
                libc.prctl(36, prior.value, 0, 0, 0)


class CommandTests(unittest.TestCase):
    target = dict(exe='authored.exe', assets='authored-assets', cd_bin='authored.BIN',
                  cd_header='legacy13', ram_top='a000', seeds=['1000:0000'], tables=[])

    def command(self):
        return module.run_command(self.target, 'native', 'saves', 'memory',
                                  SimpleNamespace(max_steps=37, key_event=['5:01:1b:down'],
                                                  virtual_step_ns=100), frame='frame')

    def test_actual_command_has_explicit_valid_clock_and_safe_devices(self):
        argv = self.command()
        self.assertEqual(argv[argv.index('--clock-centiseconds') + 1], '0')
        self.assertNotIn('4320000', argv)
        self.assertEqual(argv[argv.index('--window') + 1], 'no')
        self.assertEqual(argv[argv.index('--audio-output') + 1], 'off')
        self.assertIn('--cd-header', argv)
        self.assertIn('--key-event', argv)
        self.assertEqual(argv[-2:], ['--frame-ppm', 'frame'])

    def test_virtual_clock_invalid_before_dependencies_or_writes(self):
        for value in ('0', '-1', str(1 << 64)):
            with self.subTest(value=value), mock.patch.object(module.shutil, 'which') as which:
                with contextlib.redirect_stderr(io.StringIO()), self.assertRaises(SystemExit) as raised:
                    module.main(['--virtual-step-ns', value])
                self.assertEqual(raised.exception.code, 2)
                which.assert_not_called()

    @unittest.skipUnless(shutil.which('rustc'), 'rustc needed for actual production CLI parser')
    def test_production_startup_parser_accepts_generated_argv(self):
        # Link the real production parser + TOML implementation from the library,
        # not a std-only imitation/stub. No GUI, assets, translated game, saves,
        # or runtime execution involved. Cargo supplies the actual dependency closure.
        with tempfile.TemporaryDirectory(dir=os.environ.get('TMPDIR')) as directory:
            directory = Path(directory)
            (directory / 'src').mkdir()
            source = directory / 'src/main.rs'
            (directory / 'Cargo.toml').write_text(
                '[package]\nname="authored-startup-parser"\nversion="0.0.0"\nedition="2024"\n'
                '[dependencies]\npal-wayland={path=' + json.dumps(str(module.ROOT)) + ',default-features=false}\n')
            source.write_text(
                'fn main(){ let args:Vec<String>=std::env::args().skip(1).collect();\n'
                'if let Err(e)=pal_wayland::native_startup::Startup::for_paths(&args, std::path::Path::new("authored"), None, None)\n'
                '{eprintln!("{e}");std::process::exit(2);} }\n')
            # Poison the inherited environment: neither a player's target
            # namespace nor a cross target may escape this disposable fixture.
            foreign = directory / 'player/project/target'
            foreign.mkdir(parents=True)
            sentinel = foreign / 'debug/authored-startup-parser'
            sentinel.parent.mkdir()
            sentinel.write_bytes(b'authored player namespace sentinel')
            before = {str(p.relative_to(foreign)): p.read_bytes()
                      for p in foreign.rglob('*') if p.is_file()}
            with mock.patch.dict(os.environ, {'CARGO_TARGET_DIR': str(foreign),
                                             'CARGO_BUILD_TARGET': 'authored-invalid-cross-target'}):
                target = directory / 'target'
                env = os.environ.copy()
                env.pop('CARGO_BUILD_TARGET', None)
                env['CARGO_TARGET_DIR'] = str(target)
                compiled = subprocess.run(['cargo', 'build', '--offline', '--manifest-path', str(directory / 'Cargo.toml'), '--target-dir', str(target)],
                                          cwd=directory, env=env, text=True, capture_output=True, timeout=180)
            self.assertEqual({str(p.relative_to(foreign)): p.read_bytes()
                              for p in foreign.rglob('*') if p.is_file()}, before)
            self.assertEqual(sorted(str(p.relative_to(foreign)) for p in foreign.rglob('*')),
                             ['debug', 'debug/authored-startup-parser'])
            self.assertEqual(compiled.returncode, 0, compiled.stderr)
            executable = target / 'debug/authored-startup-parser'
            argv = self.command()[1:]
            accepted = subprocess.run([str(executable)] + argv, text=True, capture_output=True, timeout=10)
            self.assertEqual(accepted.returncode, 0, accepted.stderr)
            argv[argv.index('--clock-centiseconds') + 1] = '4320000'
            rejected = subprocess.run([str(executable)] + argv, text=True, capture_output=True, timeout=10)
            self.assertEqual(rejected.returncode, 2)
            self.assertIn('clock-centiseconds', rejected.stderr)

    def test_main_stops_build_failure_and_subprocess_failure_without_new_seeds(self):
        scenarios = ('generation', 'build', 'execution', 'timeout', 'observed-target')
        for stage in scenarios:
            with self.subTest(stage=stage), tempfile.TemporaryDirectory(dir=os.environ.get('TMPDIR')) as directory:
                root = Path(directory)
                target_file = root / 'target.json'
                target_file.write_text(json.dumps(self.target))
                calls = []
                generations = []

                def invoke(argv, cwd, timeout, *, env=None):
                    calls.append(argv)
                    code, timed_out, stderr = 0, False, ''
                    if 'pal-wayland-lift' == Path(argv[0]).name:
                        generations.append(argv)
                        if stage == 'generation': code = 7
                    elif '--manifest-path' in argv:
                        assert env is not None
                        self.assertEqual(argv[argv.index('--profile') + 1], 'dev')
                        self.assertEqual(env['CARGO_TARGET_DIR'], str(Path(argv[-1]).parent / 'target'))
                        self.assertEqual(env['CARGO_PROFILE_DEV_OVERFLOW_CHECKS'], 'true')
                        self.assertEqual(env['CARGO_PROFILE_DEV_DEBUG_ASSERTIONS'], 'true')
                        if stage == 'build': code = 7
                    elif '--' in argv and Path(argv[argv.index('--') + 1]).name == 'pal-wayland':
                        if stage == 'observed-target':
                            code = 1 if len(generations) == 1 else 0
                        else:
                            code = 124 if stage == 'timeout' else 7
                        stderr = 'pal-wayland: 1000:0010: unknown static target'
                        timed_out = stage == 'timeout'
                    return dict(exit_code=code, timed_out=timed_out, stdout='', stderr=stderr, command=argv)

                with mock.patch.object(module, 'ROOT', root), mock.patch.object(module, 'invoke', side_effect=invoke), \
                     mock.patch.object(module.shutil, 'which', return_value='/authored/tool'), \
                     contextlib.redirect_stdout(io.StringIO()), contextlib.redirect_stderr(io.StringIO()):
                    # Output parent is precreated, matching normal generated/ layout.
                    (root / 'generated').mkdir()
                    status = module.main(['--target', str(target_file), '--output', str(root / 'generated/out'),
                                          '--iterations', '2'])
                self.assertEqual(status, 0 if stage == 'observed-target' else 1)
                self.assertEqual(calls[1], ['cargo', 'build', '--offline', '--locked', '--profile', 'dev',
                                           '--bin', 'pal-wayland-lift'])
                self.assertEqual(len(generations), 2 if stage == 'observed-target' else 1)
                report = json.loads((root / 'generated/out/report.json').read_text())
                self.assertEqual(report[0]['seeds'], self.target['seeds'])
                if stage == 'observed-target':
                    self.assertIn('1000:0010', report[1]['seeds'])
                if stage in ('execution', 'timeout', 'observed-target'):
                    actual = report[0]['execution']['command']
                    self.assertEqual(Path(actual[actual.index('--') + 1]),
                                     root / 'generated/out/iteration-1/target/debug/pal-wayland')
                    self.assertEqual(actual[actual.index('--clock-centiseconds') + 1], '0')


if __name__=="__main__":unittest.main()
