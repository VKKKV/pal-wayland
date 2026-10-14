# SPDX-License-Identifier: GPL-3.0-or-later
"""Focused launcher tests; authored files only, no proprietary inputs/devices."""
import contextlib
import copy
import importlib.util
import io
import json
import os
import signal
import time
import sys
import shutil
import subprocess
from pathlib import Path
import tempfile
import unittest
from unittest import mock

SPEC = importlib.util.spec_from_file_location("native_game", Path(__file__).resolve().parents[1] / "tools/native_game.py")
assert SPEC and SPEC.loader
module = importlib.util.module_from_spec(SPEC)
sys.path.insert(0, str(Path(__file__).resolve().parents[1] / "tools"))
try:
    SPEC.loader.exec_module(module)
finally:
    sys.path.pop(0)


class NativeGameTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory(dir=os.environ.get("TMPDIR"))
        self.root = Path(self.temp.name)
        self.assets = self.root / "assets"
        self.assets.mkdir()
        # Launcher-only fixture; the resource verifier has separate CLI coverage.
        precheck = mock.patch.object(module, "verify_runtime", return_value=3)
        precheck.start()
        self.addCleanup(precheck.stop)
        self.exe = self.root / "authored.exe"
        self.exe.write_bytes(b"authored input fixture")
        self.cd = self.root / "authored.BIN"
        self.cd.write_bytes(b"authored disc fixture")
        self.cd.with_suffix(".cue").write_text("authored cue fixture")
        self.target = dict(exe=str(self.exe), assets=str(self.assets), cd_bin=str(self.cd),
                           seeds=["ABcd:0010"], tables=["ABcd:0020:03"],
                           mutable_immediates=["ABcd:0030"], ram_top="a000", cd_header="legacy13")
        self.saves = self.root / "new-saves"

    def tearDown(self):
        self.temp.cleanup()

    def test_exact_tokens_preserved_in_generation(self):
        module.validate_target(self.target)
        argv = module.generation_command(self.target, self.root / "project", self.root / "pal-wayland-lift")
        self.assertEqual(argv[1], str(self.exe))
        self.assertEqual(argv[3:], ["--fpu-macros", "borland", "--seed", "ABcd:0010",
                                  "--near-table", "ABcd:0020:03", "--mutable-immediate", "ABcd:0030"])
        target = dict(self.target, ram_top="9f00")
        module.validate_target(target)
        argv = module.run_command(target, self.exe, self.saves)
        self.assertEqual(argv[argv.index("--ram-top") + 1], "9f00")

    def test_malformed_tokens_never_normalized(self):
        for key, tokens in (("seeds", ["1:0010", "ABcd:0010 ", "0xABcd:0010", 7]),
                            ("mutable_immediates", ["1234:56789"]),
                            ("tables", ["ABcd:0020:0", "ABcd:ffff:1", "ABcd:0020:+3", "ABcd:0020:32769"])):
            for token in tokens:
                target = copy.deepcopy(self.target)
                target[key] = [token]
                with self.subTest(key=key, token=token), self.assertRaises(ValueError):
                    module.validate_target(target)
        for ram in ("0xa000", "a000 ", "a001", "1010", "9000", "9eff", True):
            target = dict(self.target, ram_top=ram)
            with self.subTest(ram=ram), self.assertRaises(ValueError):
                module.validate_target(target)

    def test_resource_paths_resolve_against_repository(self):
        with mock.patch.object(module, "ROOT", self.root):
            self.assertEqual(module.target_path("assets"), self.assets)
            target = dict(self.target, exe="authored.exe", assets="assets", cd_bin="authored.BIN")
            module.validate_target(target)
            self.assertTrue(all(Path(x).is_absolute() for x in [module.target_path(target[k]) for k in ("exe", "assets", "cd_bin")]))

    def test_default_run_is_silent_headless_and_uses_wall_clock(self):
        argv = module.run_command(self.target, self.exe, self.saves)
        self.assertEqual(argv[argv.index("--window") + 1], "no")
        self.assertEqual(argv[argv.index("--audio-output") + 1], "off")
        self.assertEqual(argv[argv.index("--sound-blaster") + 1], "no")
        self.assertEqual(argv[argv.index("--cd-header") + 1], "legacy13")
        self.assertEqual(argv[argv.index("--irq") + 1], "enabled")
        self.assertNotIn("--clock-centiseconds", argv)
        self.assertNotIn("--max-steps", argv)
        self.assertFalse(self.saves.exists())

    def test_explicit_devices_and_budgets(self):
        argv = module.run_command(self.target, self.exe, self.saves, window="yes", sound_blaster="yes", max_steps=1000)
        self.assertIn("yes", argv)
        self.assertEqual(argv[argv.index("--opl") + 1], "absent")
        self.assertEqual(argv[argv.index("--max-steps") + 1], "1000")
        argv = module.run_command(self.target, self.exe, self.saves, headless=True, virtual_step_ns=10)
        self.assertEqual(argv[-2:], ["--virtual-step-ns", "10"])
        for kwargs in ({"window": "yes", "headless": True}, {"audio_output": "pipewire", "virtual_step_ns": 1}, {"max_steps": 0}, {"max_steps": 1 << 64}, {"virtual_step_ns": -1}):
            with self.subTest(kwargs=kwargs), self.assertRaises(ValueError):
                module.run_command(self.target, self.exe, self.saves, **kwargs)

    def test_saves_reject_overlap_and_symlinks(self):
        for saves in (self.assets, self.assets / "nested", self.root):
            with self.subTest(saves=saves), self.assertRaises(ValueError):
                module.validate_saves(saves, self.assets)
        alias = self.root / "alias"
        alias.symlink_to(self.assets, target_is_directory=True)
        with self.assertRaises(ValueError):
            module.validate_saves(alias / "nested", self.assets)
        self.saves.mkdir()
        (self.saves / "SETUP.DAT").symlink_to(self.assets / "SETUP.DAT")
        with self.assertRaises(ValueError):
            module.validate_saves(self.saves, self.assets)

    def test_print_command_creates_nothing_and_never_spawns(self):
        bundle = self.root / "generated/bundle"
        executable = bundle / "project/target/debug/pal-wayland"
        executable.parent.mkdir(parents=True)
        executable.write_bytes(b"\x7fELFauthored non-executed argv fixture")
        executable.chmod(0o700)
        module.write_json(bundle / "generation.json", dict(schema=1, status="ready", target=self.target,
                          root=str(self.root), bundle=str(bundle), project=str(bundle / "project"),
                          executable=str(executable), asset_sha256={},
                          executable_sha256=module.digest(executable),
                          input_sha256={str(p): module.digest(p) for p in
                                        (self.exe, self.cd, self.cd.with_suffix('.cue'))}))
        output = io.StringIO()
        with contextlib.redirect_stdout(output), mock.patch.object(module, "ROOT", self.root), \
             mock.patch.object(module.subprocess, "run") as spawn:
            result = module.main(["run", "--project", str(bundle), "--saves", str(self.saves), "--print-command"])
        self.assertEqual(result, 0)
        self.assertFalse(self.saves.exists())
        spawn.assert_not_called()
        self.assertEqual(json.loads(output.getvalue()), module.run_command(self.target, executable, self.saves))

    def test_inherited_cargo_output_is_overridden(self):
        with mock.patch.dict(os.environ, {"CARGO_TARGET_DIR": "/unrelated", "CARGO_BUILD_TARGET": "wrong-target"}):
            env = module.build_environment(self.root)
        self.assertEqual(env["CARGO_TARGET_DIR"], str(self.root / "target"))
        self.assertNotIn("CARGO_BUILD_TARGET", env)
        self.assertEqual(env["CARGO_PROFILE_DEV_OPT_LEVEL"], "1")
        self.assertEqual(env["CARGO_PROFILE_DEV_DEBUG"], "0")
        self.assertEqual(env["CARGO_INCREMENTAL"], "0")

    def test_profile_selection_and_checks_override_inherited_profiles(self):
        self.assertEqual(module.parser().parse_args(['build']).profile, 'player')
        self.assertEqual(module.parser().parse_args(['build', '--profile', 'dev']).profile, 'dev')
        inherited = {'CARGO_PROFILE_DEV_OPT_LEVEL': '0',
                     'CARGO_PROFILE_DEV_OVERFLOW_CHECKS': 'false',
                     'CARGO_PROFILE_DEV_DEBUG_ASSERTIONS': 'false',
                     'CARGO_PROFILE_RELEASE_LTO': 'true'}
        for profile, opt in (('player', '3'), ('dev', '1')):
            with self.subTest(profile=profile), mock.patch.dict(os.environ, inherited):
                env = module.build_environment(self.root, profile=profile)
                self.assertEqual(env['CARGO_PROFILE_DEV_OPT_LEVEL'], opt)
                self.assertEqual(env['CARGO_PROFILE_DEV_OVERFLOW_CHECKS'], 'true')
                self.assertEqual(env['CARGO_PROFILE_DEV_DEBUG_ASSERTIONS'], 'true')
                self.assertEqual(env['CARGO_PROFILE_DEV_CODEGEN_UNITS'], '16')
                self.assertEqual(env['CARGO_PROFILE_DEV_LTO'], 'false')
                self.assertNotIn('CARGO_PROFILE_RELEASE_LTO', env)
        with self.assertRaises(ValueError):
            module.build_environment(self.root, profile='release')

    def test_conflicting_rustflags_rejected_before_bundle_creation(self):
        target = self.root / 'target.json'
        target.write_text(json.dumps(self.target))
        output = self.root / 'generated/rejected-flags'
        for key in ('RUSTFLAGS', 'CARGO_ENCODED_RUSTFLAGS', 'CARGO_BUILD_RUSTFLAGS',
                    'CARGO_TARGET_X86_64_UNKNOWN_LINUX_GNU_RUSTFLAGS'):
            with self.subTest(key=key), mock.patch.dict(os.environ, {key: '-Coverflow-checks=no'}), \
                 mock.patch.object(module, 'ROOT', self.root), mock.patch.object(module, 'invoke') as spawn, \
                 contextlib.redirect_stderr(io.StringIO()):
                self.assertEqual(module.main(['build', '--target', str(target), '--output', str(output)]), 2)
                spawn.assert_not_called()
                self.assertFalse(output.exists())

    @unittest.skipUnless(shutil.which('cargo'), 'cargo needed for real profile compilation')
    def test_real_cargo_profiles_optimize_and_keep_runtime_checks(self):
        # Authored, dependency-free code. Actually compile and execute both
        # configurations; verbose rustc argv independently proves Cargo's flags.
        for profile, opt in (('player', '3'), ('dev', '1')):
            with self.subTest(profile=profile):
                project = self.root / profile
                (project / 'src').mkdir(parents=True)
                (project / 'Cargo.toml').write_text(
                    '[package]\nname="profile-check"\nversion="0.0.0"\nedition="2024"\n[workspace]\n')
                (project / 'src/main.rs').write_text(
                    'fn main() {\n'
                    '  assert!(cfg!(debug_assertions));\n'
                    '  std::panic::set_hook(Box::new(|_| {}));\n'
                    '  let x = std::hint::black_box(u32::MAX);\n'
                    '  assert!(std::panic::catch_unwind(|| x + 1).is_err());\n'
                    '  assert!(std::panic::catch_unwind(|| debug_assert!(false)).is_err());\n'
                    '  println!("checked-profile-ok");\n}\n')
                env = module.build_environment(project, profile=profile)
                built = subprocess.run(['cargo', 'build', '--offline', '--profile', 'dev', '-vv',
                                        '--manifest-path', str(project / 'Cargo.toml')],
                                       env=env, text=True, capture_output=True, timeout=60)
                self.assertEqual(built.returncode, 0, built.stderr)
                self.assertIn(f'-C opt-level={opt}', built.stderr)
                self.assertIn('-C debug-assertions=on', built.stderr)
                self.assertNotIn('-C overflow-checks=off', built.stderr)
                # Cargo may omit explicit overflow-checks=on when assertions
                # imply it; actual execution below verifies overflow still traps.
                self.assertIn('-C codegen-units=16', built.stderr)
                ran = subprocess.run([str(project / 'target/debug/profile-check')],
                                     text=True, capture_output=True, timeout=10)
                self.assertEqual(ran.returncode, 0, ran.stderr)
                self.assertEqual(ran.stdout.strip(), 'checked-profile-ok')

    def test_complete_build_records_selected_profile_and_actual_command(self):
        target = self.root / 'target.json'
        target.write_text(json.dumps(self.target))
        for profile in ('player', 'dev'):
            output = self.root / 'generated' / profile
            project = output / 'project'
            calls = []

            def invoke(argv, bundle, label, timeout, env, *, output_limit_bytes=None):
                calls.append((label, argv, env))
                if label.startswith('identity-'):
                    self.assertEqual(output_limit_bytes, module.IDENTITY_OUTPUT_LIMIT_BYTES)
                    # Explicit authored identity subprocess responses, not live versions.
                    text = ('rustc authored-fixture\nhost: authored-host\nrelease: authored-release\n'
                            if label == 'identity-rustc' else 'authored-version\n')
                    stdout, stderr = bundle / f'{label}.stdout.log', bundle / f'{label}.stderr.log'
                    stdout.write_text(text)
                    missing_optional = label == 'identity-vulkan'
                    stderr.write_text('authored missing optional pkg-config metadata' if missing_optional else '')
                    return dict(exit_code=1 if missing_optional else 0, timed_out=False,
                                command=argv, stdout=str(stdout), stderr=str(stderr))
                self.assertIn('build_identity', json.loads((bundle.parent / 'generation.json').read_text()))
                if label == 'generate':
                    (project / 'src').mkdir(parents=True)
                    (project / 'Cargo.toml').write_text('[workspace]\n')
                    (project / 'src/main.rs').write_text('// authored test\n')
                    (project / 'src/translated.rs').write_text('// authored test\n')
                    (project / 'source-map.txt').write_text('# authored test\n')
                    (project / 'SCOPE.txt').write_text('authored test\n')
                elif label == 'lock':
                    (project / 'Cargo.lock').write_text('# authored test\n')
                elif label == 'native-build':
                    self.assertEqual(argv[argv.index('--profile') + 1], 'dev')
                    self.assertEqual(argv[argv.index('--features') + 1],
                                     'window,profiling' if profile == 'player' else 'window')
                    self.assertEqual(env['CARGO_PROFILE_DEV_OPT_LEVEL'], '3' if profile == 'player' else '1')
                    elf = Path(env['CARGO_TARGET_DIR']) / 'debug/pal-wayland'
                    elf.parent.mkdir(parents=True)
                    shutil.copyfile(sys.executable, elf)  # Real ELF, never executed as a game.
                    elf.chmod(0o700)
                return dict(exit_code=0, timed_out=False, command=argv)

            with self.subTest(profile=profile), mock.patch.object(module, 'ROOT', self.root), \
                 mock.patch.object(module, 'source_snapshot', return_value={}), \
                 mock.patch.object(module.shutil, 'which', side_effect=lambda tool, **kwargs: '/authored/' + tool), \
                 mock.patch.object(module, 'invoke', side_effect=invoke), \
                 mock.patch.object(module, 'project_inventory', wraps=module.project_inventory) as inventory, \
                 contextlib.redirect_stdout(io.StringIO()), contextlib.redirect_stderr(io.StringIO()):
                self.assertEqual(module.main(['build', '--profile', profile, '--target', str(target),
                                              '--output', str(output), '--build-timeout', '1800'] +
                                             (['--profiling'] if profile == 'player' else [])), 0)
                # Post-lock and post-build checks remain; publication validates
                # the full bundle (including project/) through schema2_structure.
                self.assertEqual(inventory.call_count, 2)
                record = json.loads((output / 'generation.json').read_text())
                self.assertEqual(record['status'], 'ready')
                self.assertEqual(record['profile'], module.build_profile(profile))
                self.assertEqual(record['features'], ['window', 'profiling'] if profile == 'player' else ['window'])
                self.assertEqual(record['steps'][-1]['environment'], record['build_environment'])
                self.assertEqual(record['schema'], 2)
                self.assertEqual(record['executable'], str(output / 'bin/pal-wayland'))
                self.assertEqual(record['build_cache'], str(self.root / 'target/native' / profile))
                self.assertEqual(record['logs'], str(output / 'logs'))
                self.assertEqual(record['build_environment']['CARGO_TARGET_DIR'], record['build_cache'])
                module.ready_player(output)
                self.assertEqual(set(record['generated_sha256']), set(module.GENERATED_FILES))
                self.assertFalse((project / 'target').exists())
                self.assertNotEqual(Path(record['executable']).stat().st_ino,
                                    (Path(record['build_cache']) / 'debug/pal-wayland').stat().st_ino)
                self.assertEqual(record['steps'][0]['command'],
                                 ['cargo', 'build', '--offline', '--locked', '--bin', 'pal-wayland-lift'])
                self.assertEqual(record['steps'][1]['command'][0], str(self.root / 'target/debug/pal-wayland-lift'))
                self.assertEqual(record['executable_sha256'], module.digest(Path(record['executable'])))
                identity = record['build_identity']
                self.assertEqual(identity['rustc']['release'], 'authored-release')
                self.assertEqual(identity['rustc']['host_target'], 'authored-host')
                self.assertEqual(identity['cargo']['version'], 'authored-version')
                self.assertEqual(identity['cargo']['executable'], '/authored/cargo')
                self.assertIsNone(identity['cargo_build_target'])
                libraries = identity['system_libraries']
                self.assertEqual(libraries['libpipewire-0.3']['version'], 'authored-version')
                self.assertTrue(libraries['libspa-0.2']['required_at_build'])
                self.assertFalse(libraries['wayland-client']['required_at_build'])
                self.assertEqual(libraries['vulkan']['status'], 'failed')
                self.assertEqual(libraries['vulkan']['exit_code'], 1)
                self.assertIsNone(libraries['vulkan']['version'])
                self.assertEqual([c[0] for c in calls],
                                 ['identity-rustc', 'identity-cargo', 'identity-libpipewire-0.3',
                                  'identity-libspa-0.2', 'identity-wayland-client',
                                  'identity-xkbcommon', 'identity-vulkan',
                                  'tool-build', 'generate', 'lock', 'native-build'])
                self.assertFalse(record['spawned_game'])

    def test_identity_missing_tools_are_explicit_and_do_not_spawn(self):
        with mock.patch.object(module.shutil, 'which', return_value=None), \
             mock.patch.object(module, 'invoke') as spawn:
            identity = module.build_identity(self.root, module.build_environment(), 600, ['window'])
        spawn.assert_not_called()
        for probe in (identity['rustc'], identity['cargo'], *identity['system_libraries'].values()):
            self.assertEqual(probe['status'], 'unavailable')
            self.assertIsNone(probe['version'])
            self.assertIsNone(probe['executable'])
            self.assertIsNone(probe['exit_code'])
            self.assertIn('not found', probe['error'])
        self.assertIsNone(identity['rustc']['host_target'])
        self.assertIsNone(identity['rustc']['release'])

    def test_identity_probes_capture_actual_output_failure_and_timeout(self):
        env = os.environ.copy()
        for label, source, code in (
                ('success', "print('authored identity probe')", 0),
                ('failure', "import sys;print('not a version');print('authored failure',file=sys.stderr);sys.exit(7)", 7),
                ('timeout', "import time;time.sleep(10)", 124)):
            with self.subTest(label=label):
                result = module.identity_probe(sys.executable, ['-c', source], self.root,
                                               label, 0.05 if code == 124 else 10, env)
                self.assertEqual(result['exit_code'], code)
                self.assertEqual(result['timed_out'], code == 124)
                self.assertEqual(result['executable'], sys.executable)
                self.assertEqual(result['command'], [sys.executable, '-c', source])
                self.assertTrue(Path(result['stdout']).is_file())
                self.assertTrue(Path(result['stderr']).is_file())
                if code == 0:
                    self.assertEqual(result['status'], 'known')
                    self.assertEqual(result['version'], 'authored identity probe')
                else:
                    self.assertEqual(result['status'], 'failed')
                    self.assertIsNone(result['version'])
                    self.assertIn('timed out' if code == 124 else 'authored failure', result['error'])
        with mock.patch.object(module, 'invoke', side_effect=OSError('authored spawn failure')):
            result = module.identity_probe(sys.executable, ['-V'], self.root, 'spawn-failure', 10, env)
        self.assertEqual(result['status'], 'failed')
        self.assertIsNone(result['exit_code'])
        self.assertIsNone(result['version'])
        self.assertEqual(result['error'], 'authored spawn failure')

    def test_identity_capture_bounds_timeout_and_selects_feature_libraries(self):
        def probe(tool, arguments, bundle, label, timeout, env):
            self.assertEqual(timeout, 10)
            self.assertEqual(bundle, self.root)
            return {'version': None, 'status': 'unavailable'}

        env = module.build_environment()
        with mock.patch.object(module, 'identity_probe', side_effect=probe) as capture:
            identity = module.build_identity(self.root, env, 1800, [])
        self.assertEqual(list(identity['system_libraries']), ['libpipewire-0.3', 'libspa-0.2'])
        self.assertEqual([call.args[1] for call in capture.call_args_list],
                         [['-Vv'], ['-V'], ['--modversion', 'libpipewire-0.3'],
                          ['--modversion', 'libspa-0.2']])

    def test_identity_interruption_preserves_failed_generation(self):
        target = self.root / 'target.json'
        target.write_text(json.dumps(self.target))
        output = self.root / 'generated/interrupted-identity'
        with mock.patch.object(module, 'ROOT', self.root), \
             mock.patch.object(module, 'source_snapshot', return_value={}), \
             mock.patch.object(module, 'build_identity', side_effect=KeyboardInterrupt), \
             mock.patch.object(module, 'invoke') as spawn:
            self.assertEqual(module.main(['build', '--target', str(target), '--output', str(output)]), 130)
        spawn.assert_not_called()
        self.assertEqual(json.loads((output / 'generation.json').read_text())['status'], 'failed')

    @unittest.skipUnless(sys.platform == 'linux', 'Linux process-group and adoption regression')
    def test_owned_group_cleans_term_ignoring_descendants(self):
        # Adopt orphaned authored descendants only during this test, so we can
        # reap them rather than leave test zombies to an unreliable PID 1.
        import ctypes
        import threading
        libc = ctypes.CDLL(None, use_errno=True)
        prior = ctypes.c_int()
        self.assertEqual(libc.prctl(37, ctypes.byref(prior), 0, 0, 0), 0)
        self.assertEqual(libc.prctl(36, 1, 0, 0, 0), 0)
        try:
            for bounded in (False, True):
                modes = ('timeout', 'interrupt', 'success', 'overflow') if bounded else ('timeout', 'interrupt', 'success')
                for mode in modes:
                    with self.subTest(bounded=bounded, mode=mode):
                        pidfile = self.root / f'child-{bounded}-{mode}.pid'
                        child = ("import os,signal,time;from pathlib import Path;"
                                 "signal.signal(signal.SIGTERM,signal.SIG_IGN);"
                                 f"Path({str(pidfile)!r}).write_text(str(os.getpid()));"
                                 "time.sleep(600)")
                        leader = ("import subprocess,sys,time;from pathlib import Path;"
                                  f"subprocess.Popen([sys.executable,'-c',{child!r}]);\n"
                                  f"while not Path({str(pidfile)!r}).exists(): time.sleep(.005)\n" +
                                  ("sys.exit(0)" if mode == 'success' else
                                   "import os;os.write(1,b'X'*(1024*1024));time.sleep(600)" if mode == 'overflow' else
                                   "time.sleep(600)"))
                        procs = []
                        original_popen = subprocess.Popen

                        def spawn(*args, **kwargs):
                            proc = original_popen(*args, **kwargs)
                            procs.append(proc)
                            return proc

                        # A separate session must remain completely untouched.
                        control = original_popen([sys.executable, '-c', 'import time;time.sleep(600)'],
                                                 start_new_session=True)
                        thread = None
                        stop = threading.Event()
                        if mode == 'interrupt':
                            def interrupt_when_ready():
                                end = time.monotonic() + 5
                                while not stop.wait(.005) and time.monotonic() < end:
                                    if pidfile.exists():
                                        os.kill(os.getpid(), signal.SIGINT)
                                        return
                            thread = threading.Thread(target=interrupt_when_ready)
                            thread.start()
                        try:
                            with mock.patch.object(module.subprocess, 'Popen', side_effect=spawn):
                                kwargs = {'output_limit_bytes': 65536} if bounded else {}
                                if mode == 'interrupt':
                                    with self.assertRaises(KeyboardInterrupt):
                                        module.invoke([sys.executable, '-c', leader], self.root,
                                                      f'group-{bounded}-{mode}', 5, os.environ.copy(), **kwargs)
                                else:
                                    result = module.invoke([sys.executable, '-c', leader], self.root,
                                                           f'group-{bounded}-{mode}', 1, os.environ.copy(), **kwargs)
                                    # Bounded success holds inherited pipes open:
                                    # it must time out, not wait forever for EOF.
                                    timed_out = mode == 'timeout' or (bounded and mode == 'success')
                                    self.assertEqual(result['timed_out'], timed_out)
                                    self.assertEqual(result['exit_code'], 125 if mode == 'overflow' else 124 if timed_out else 0)
                                    self.assertEqual(result['output_limit_exceeded'], mode == 'overflow')
                            self.assertTrue(pidfile.exists(), 'descendant fixture never started')
                            pid = int(pidfile.read_text())
                            end = time.monotonic() + 2
                            while time.monotonic() < end:
                                info = Path(f'/proc/{pid}/stat')
                                if not info.exists() or info.read_text().split(') ', 1)[1].startswith('Z '):
                                    break
                                time.sleep(.01)
                            else:
                                self.fail('SIGTERM-ignoring descendant still running after cleanup')
                            self.assertIsNotNone(procs[0].poll(), 'owned leader was not reaped')
                            self.assertIsNone(control.poll(), 'unrelated process group was signalled')
                        finally:
                            stop.set()
                            if thread is not None:
                                thread.join(timeout=5)
                            for proc in procs:
                                try:
                                    os.killpg(proc.pid, signal.SIGKILL)
                                except ProcessLookupError:
                                    pass
                                proc.wait(timeout=5)
                            if pidfile.exists():
                                pid = int(pidfile.read_text())
                                try:
                                    os.kill(pid, signal.SIGKILL)
                                except ProcessLookupError:
                                    pass
                                try:
                                    os.waitpid(pid, 0)
                                except ChildProcessError:
                                    pass
                            control.kill()
                            control.wait(timeout=5)
        finally:
            self.assertEqual(libc.prctl(36, prior.value, 0, 0, 0), 0)

    def test_identity_rejects_real_oversized_streams_and_preserves_prefixes(self):
        for stream in ('stdout', 'stderr'):
            with self.subTest(stream=stream):
                source = ("import os;os.write(1,b'authored-version\\n');"
                          f"os.write({1 if stream == 'stdout' else 2},b'X'*(1024*1024))")
                result = module.identity_probe(sys.executable, ['-c', source], self.root,
                                               'huge-' + stream, 10, os.environ.copy())
                self.assertEqual(result['status'], 'failed')
                self.assertIsNone(result['version'])
                self.assertTrue(result['output_limit_exceeded'])
                self.assertFalse(result['logs_complete'])
                self.assertEqual(result['exit_code'], 125)
                self.assertIn('output_limit_exceeded', result['error'])
                self.assertLessEqual(len(result['error'].encode()), 4096)
                for key in ('stdout', 'stderr'):
                    self.assertLessEqual(Path(result[key]).stat().st_size, 65536)
                self.assertEqual(Path(result[stream]).stat().st_size, 65536)

    def test_identity_version_and_summary_budgets_are_explicit(self):
        for size in (16384, 16385):
            with self.subTest(size=size):
                result = module.identity_probe(sys.executable, ['-c', f"import os;os.write(1,b'X'*{size})"],
                                               self.root, f'version-{size}', 10, os.environ.copy())
                self.assertEqual(result['status'], 'known' if size == 16384 else 'failed')
                self.assertEqual(result['version_limit_exceeded'], size > 16384)
                self.assertFalse(result['output_limit_exceeded'])
                self.assertTrue(result['logs_complete'])
                self.assertEqual(result['read_limit_bytes'], 65537)
                self.assertEqual(result['version_limit_bytes'], 16384)
                self.assertEqual(result['summary_limit_bytes'], 4096)
                if size > 16384:
                    self.assertIsNone(result['version'])
                    self.assertIn('version_limit_exceeded', result['error'])
        result = module.identity_probe(sys.executable, ['-c',
                                       "import os,sys;os.write(2,'错误'.encode()*6000);sys.exit(7)"],
                                       self.root, 'huge-summary', 10, os.environ.copy())
        self.assertEqual(result['status'], 'failed')
        self.assertTrue(result['summary_limit_exceeded'])
        self.assertLessEqual(len(result['error'].encode()), 4096)
        self.assertGreater(Path(result['stderr']).stat().st_size, 4096)

    def test_identity_reads_only_budget_plus_sentinel_even_if_log_is_oversized(self):
        stdout, stderr = self.root / 'oversized.log', self.root / 'empty.log'
        stdout.write_bytes(b'X' * (1024 * 1024))
        stderr.write_bytes(b'')
        result = dict(exit_code=0, timed_out=False, stdout=str(stdout), stderr=str(stderr))
        original_open = Path.open
        reads = []

        class Reader:
            def __init__(self, path):
                self.stream = original_open(path, 'rb')
            def __enter__(self):
                return self
            def __exit__(self, *args):
                self.stream.close()
            def read(self, size=-1):
                reads.append(size)
                return self.stream.read(size)

        with mock.patch.object(module, 'invoke', return_value=result), \
             mock.patch.object(Path, 'open', side_effect=lambda path, *args, **kwargs: Reader(path), autospec=True):
            probe = module.identity_probe(sys.executable, ['-V'], self.root, 'read-budget', 10, os.environ.copy())
        self.assertEqual(reads, [65537, 65537])
        self.assertEqual(probe['status'], 'failed')
        self.assertTrue(probe['output_limit_exceeded'])
        self.assertIsNone(probe['version'])

    def test_bounded_invoke_exact_limits_and_ordinary_build_logs_are_complete(self):
        source = "import os;os.write(1,b'A'*65536);os.write(2,b'B'*65536)"
        result = module.invoke([sys.executable, '-c', source], self.root, 'exact-budget', 10,
                               os.environ.copy(), output_limit_bytes=65536)
        self.assertEqual(result['exit_code'], 0)
        self.assertFalse(result['output_limit_exceeded'])
        self.assertTrue(result['logs_complete'])
        source = "import os;os.write(1,b'A'*(1024*1024));os.write(2,b'B'*(1024*1024))"
        result = module.invoke([sys.executable, '-c', source], self.root, 'full-build-log', 10, os.environ.copy())
        self.assertEqual(result['exit_code'], 0)
        self.assertIsNone(result['output_limit_bytes'])
        self.assertTrue(result['logs_complete'])
        self.assertEqual(Path(result['stdout']).read_bytes(), b'A' * (1024 * 1024))
        self.assertEqual(Path(result['stderr']).read_bytes(), b'B' * (1024 * 1024))
        for limit in (0, -1, True, 1.5, '65536'):
            with self.subTest(limit=limit), self.assertRaises(ValueError), \
                 mock.patch.object(module.subprocess, 'Popen') as spawn:
                module.invoke([], self.root, 'invalid-budget', 10, os.environ.copy(), output_limit_bytes=limit)
            spawn.assert_not_called()

    def test_existing_build_rejected_without_spawning(self):
        target = self.root / "target.json"
        target.write_text(json.dumps(self.target))
        output = self.root / "generated/native-existing"
        output.mkdir(parents=True)
        with mock.patch.object(module, "ROOT", self.root), mock.patch.object(module, "invoke") as spawn, contextlib.redirect_stderr(io.StringIO()):
            code = module.main(["build", "--target", str(target), "--output", str(output)])
        self.assertEqual(code, 2)
        spawn.assert_not_called()
        output = self.root / 'generated/cache-collision'
        cache = self.root / 'target/native/cache-collision'
        cache.mkdir(parents=True)
        sentinel = cache / 'user-file'
        sentinel.write_bytes(b'authored unrelated cache')
        with mock.patch.object(module, 'ROOT', self.root), mock.patch.object(module, 'invoke') as spawn, \
             contextlib.redirect_stderr(io.StringIO()):
            self.assertEqual(module.main(['build', '--target', str(target), '--output', str(output)]), 2)
        spawn.assert_not_called()
        self.assertFalse(output.exists())
        self.assertEqual(sentinel.read_bytes(), b'authored unrelated cache')

    def test_failed_build_preserves_manifest_and_real_output(self):
        target = self.root / "target.json"
        target.write_text(json.dumps(self.target))
        output = self.root / "generated/native-failure"
        original_invoke = module.invoke
        def actual_failure(command, bundle, label, timeout, env, **kwargs):
            error = "X" * 16000 + "authored error" if label == "tool-build" else "authored error"
            source = f"import sys; print('authored build diagnostic'); print({error!r}, file=sys.stderr); sys.exit(7)"
            return original_invoke([sys.executable, "-c", source], bundle, label, timeout, env, **kwargs)
        diagnostic = io.StringIO()
        with mock.patch.object(module, "ROOT", self.root), mock.patch.object(module, "source_snapshot", return_value={}), mock.patch.object(module, "invoke", side_effect=actual_failure), contextlib.redirect_stdout(io.StringIO()), contextlib.redirect_stderr(diagnostic):
            code = module.main(["build", "--target", str(target), "--output", str(output)])
        self.assertEqual(code, 7)
        record = json.loads((output / "generation.json").read_text())
        self.assertEqual(record["status"], "failed")
        self.assertEqual(record['build_identity']['rustc']['status'], 'failed')
        self.assertIsNone(record['build_identity']['rustc']['version'])
        self.assertEqual(record['build_identity']['cargo']['exit_code'], 7)
        self.assertEqual(record["target"], self.target)
        self.assertEqual(record["profile"], module.build_profile("player"))
        self.assertEqual(record["build_environment"]["CARGO_PROFILE_DEV_OPT_LEVEL"], "3")
        self.assertEqual(record["build_environment"]["CARGO_PROFILE_DEV_OVERFLOW_CHECKS"], "true")
        self.assertEqual((output / "logs/tool-build.stdout.log").read_text().strip(), "authored build diagnostic")
        self.assertEqual((output / "logs/tool-build.stderr.log").read_text().strip(), "X" * 16000 + "authored error")
        self.assertTrue(diagnostic.getvalue().endswith("authored error\n\n"))
        self.assertLess(len(diagnostic.getvalue()), 9000)


class PlayerAssemblyTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory(dir=os.environ.get("TMPDIR"))
        self.root = Path(self.temp.name) / "original-repo"
        self.root.mkdir()
        self.patch = mock.patch.object(module, "ROOT", self.root)
        self.patch.start()
        self.addCleanup(self.patch.stop)
        self.addCleanup(self.temp.cleanup)
        assets = self.root / "assets/dos-cd"
        assets.mkdir(parents=True)
        (assets / "authored.dat").write_bytes(b"authored resource")
        exe = self.root / "assets/unpacked/PAL.raw.exe"
        exe.parent.mkdir()
        exe.write_bytes(b"original authored input")
        cd = self.root / "assets/GC050.BIN"
        cd.write_bytes(b"original authored disc")
        cd.with_suffix(".cue").write_text("original authored cue")
        self.target: dict = dict(exe=str(exe), assets=str(assets), cd_bin=str(cd), seeds=[], tables=[])
        self.source = self.root / "UI-SETUP.DAT"
        self.source.write_bytes(bytes(range(20)))  # Authored, not copied game bytes.
        self.old, self.old_elf = self.bundle("old")
        self.new, self.new_elf = self.bundle("new")
        self.entry = self.root / "pal"
        self.entry.symlink_to(self.old_elf.relative_to(self.root))
        self.previous = os.readlink(self.entry)

    def bundle(self, name, *, player_name='pal-wayland', schema=1):
        bundle = self.root / "generated" / name
        elf = bundle / (module.SCHEMA2_PLAYER_PATH if schema == 2 else Path("project/target/debug") / player_name)
        elf.parent.mkdir(parents=True)
        elf.write_bytes(b"\x7fELFauthored nonexecuted assembly fixture " + name.encode())
        elf.chmod(0o700)
        resources = [Path(self.target['exe']), Path(self.target['cd_bin']), Path(self.target['cd_bin']).with_suffix('.cue')]
        record = dict(
            schema=schema, status="ready", root=str(self.root), bundle=str(bundle),
            project=str(bundle / "project"), executable=str(elf),
            executable_sha256=module.digest(elf), target=self.target,
            asset_sha256={str(p): module.digest(p) for p in Path(self.target['assets']).rglob('*') if p.is_file()},
            input_sha256={str(p): module.digest(p) for p in resources})
        if schema == 2:
            shutil.copyfile(sys.executable, elf)
            elf.chmod(0o700)
            record['executable_sha256'] = module.digest(elf)
            cache = self.root / 'target/native' / name
            record.update(logs=str(bundle / 'logs'), build_cache=str(cache),
                          profile=module.build_profile('player'),
                          build_environment=module.recorded_environment(
                              module.build_environment(bundle / 'project', profile='player', cache=cache)))
            (bundle / 'logs').mkdir()
            for filename in module.GENERATED_FILES:
                path = bundle / 'project' / filename
                path.parent.mkdir(parents=True, exist_ok=True)
                path.write_text('authored generated fixture ' + filename)
            record['generated_sha256'] = {filename: module.digest(bundle / 'project' / filename)
                                          for filename in module.GENERATED_FILES}
        module.write_json(bundle / "generation.json", record)
        return bundle, elf

    def assemble(self, **options):
        values = dict(project=self.new, player_config=self.source, entry=self.entry)
        values.update(options)
        argv = ["assemble"]
        for key, value in values.items():
            argv.extend(("--" + key.replace("_", "-"), str(value)))
        with contextlib.redirect_stdout(io.StringIO()), contextlib.redirect_stderr(io.StringIO()):
            return module.main(argv)

    def assert_old(self):
        self.assertEqual(os.readlink(self.entry), self.previous)
        self.assertEqual(self.entry.resolve(), self.old_elf)

    def test_exact_seed_provenance_atomic_link_and_idempotent_retry(self):
        env = dict(os.environ)
        env.pop("TMPDIR", None)
        with mock.patch.dict(os.environ, env, clear=True), \
             mock.patch.object(module, "invoke") as build, mock.patch.object(module.subprocess, "run") as spawn:
            self.assertEqual(self.assemble(), 0)
            self.assertEqual(self.assemble(), 0)
        build.assert_not_called()
        spawn.assert_not_called()
        self.assertEqual(self.entry.resolve(), self.new_elf)
        seed = self.new / "player-config/SETUP.DAT"
        self.assertEqual(seed.read_bytes(), self.source.read_bytes())
        info = json.loads((self.new / "assembly.json").read_text())
        self.assertEqual(info['player_config']['source'], str(self.source))
        self.assertEqual(info['player_config']['sha256'], module.digest(self.source))
        self.assertEqual(info['generation_sha256'], module.digest(self.new / "generation.json"))
        self.assertEqual(info['executable_sha256'], module.digest(self.new_elf))
        self.assertFalse(info['spawned_game'])

    def test_old_schema_one_bundle_remains_a_rollback_target(self):
        # Deliberate old-schema input, not a newly generated player's name.
        self.old, self.old_elf = self.bundle('legacy', player_name='pal-native-generated')
        self.entry.unlink()
        self.entry.symlink_to(self.old_elf.relative_to(self.root))
        self.previous = os.readlink(self.entry)
        self.new, self.new_elf = self.bundle('schema2', schema=2)
        path = self.new / 'generation.json'
        record = json.loads(path.read_text())
        record['build_identity'] = {'system_libraries': {'vulkan': {'status': 'unavailable', 'version': None}}}
        module.write_json(path, record)
        self.assertEqual(self.assemble(), 0)
        self.assertEqual(self.entry.resolve(), self.new_elf)
        old_record = json.loads((self.old / 'generation.json').read_text())
        self.assertNotIn('build_identity', old_record)
        with mock.patch.object(module, 'build_identity') as probe, \
             mock.patch.object(module, 'source_snapshot') as snapshot:
            self.assertEqual(self.assemble(project=self.old), 0)
        probe.assert_not_called()
        snapshot.assert_not_called()
        self.assert_old()

        # Move a complete authored repository, including assembled players, then
        # exercise the actual CLI without patching its ROOT or entry functions.
        fresh, fresh_elf = self.bundle('unassembled')
        # Newer schema1 records carry exactly these generated outputs. A
        # missing field stays compatible; a present incomplete map must fail.
        for name in module.GENERATED_FILES:
            path = fresh / 'project' / name
            path.parent.mkdir(parents=True, exist_ok=True)
            path.write_text('authored generated fixture ' + name)
        fresh_record = json.loads((fresh / 'generation.json').read_text())
        fresh_record['generated_sha256'] = {name: module.digest(fresh / 'project' / name)
                                           for name in module.GENERATED_FILES}
        module.write_json(fresh / 'generation.json', fresh_record)
        tools = self.root / 'tools'
        tools.mkdir()
        for name in ('native_game.py', 'verify_assets.py', 'inspect_mz.py'):
            shutil.copyfile(Path(__file__).resolve().parents[1] / 'tools' / name, tools / name)
        old_root = self.root
        frozen = {p.relative_to(old_root): p.read_bytes() for bundle in (self.old, self.new)
                  for p in (bundle / 'generation.json', bundle / 'assembly.json')}
        moved = old_root.with_name('relocated-repo')
        old_root.rename(moved)
        self.root = moved
        for name in ('old', 'old_elf', 'new', 'new_elf', 'source', 'entry'):
            setattr(self, name, moved / getattr(self, name).relative_to(old_root))
        setattr(module, 'ROOT', moved)
        cli = [sys.executable, str(moved / 'tools/native_game.py')]
        argv = ['assemble', '--project', str(self.new), '--player-config', str(self.source)]
        result = subprocess.run(cli + argv, capture_output=True, text=True, timeout=20)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(self.entry.resolve(), self.new_elf)
        for relative, original in frozen.items():
            self.assertEqual((moved / relative).read_bytes(), original)
        fresh = moved / fresh.relative_to(old_root)
        fresh_elf = moved / fresh_elf.relative_to(old_root)
        fresh_argv = ['assemble', '--project', str(fresh), '--player-config', str(self.source)]
        result = subprocess.run(cli + fresh_argv, capture_output=True, text=True, timeout=20)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(self.entry.resolve(), fresh_elf)
        self.assertEqual(json.loads((fresh / 'assembly.json').read_text())['bundle'], str(fresh))
        generated = fresh / 'generation.json'
        generated_bytes = generated.read_bytes()
        valid_generated = json.loads(generated_bytes)
        inventories = [{}, {k:v for k,v in valid_generated['generated_sha256'].items()
                            if k != 'src/translated.rs'},
                       dict(valid_generated['generated_sha256'], **{'private-not-generated.txt':'0'*64})]
        for inventory in inventories:
            bad_generated = copy.deepcopy(valid_generated)
            bad_generated['generated_sha256'] = inventory
            module.write_json(generated, bad_generated)
            with mock.patch.object(module, 'digest', wraps=module.digest) as hashes:
                with self.assertRaisesRegex(ValueError, 'generated file inventory'):
                    module.ready_player(fresh)
                self.assertNotIn(mock.call(fresh / 'project/private-not-generated.txt'), hashes.call_args_list)
            generated.write_bytes(generated_bytes)
        module.ready_player(fresh)
        result = subprocess.run(cli + argv, capture_output=True, text=True, timeout=20)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(self.entry.resolve(), self.new_elf)
        saves = moved / 'uncreated-saves'
        run_argv = ['run', '--project', str(self.new), '--saves', str(saves), '--print-command']
        result = subprocess.run(cli + run_argv, capture_output=True, text=True, timeout=20)
        self.assertEqual(result.returncode, 0, result.stderr)
        command = json.loads(result.stdout)
        self.assertEqual(command[0], str(self.new_elf))
        self.assertEqual(command[1], str(moved / 'assets/unpacked/PAL.raw.exe'))
        self.assertFalse(saves.exists())
        # Both exact filenames survive relocation with frozen metadata intact.
        legacy_argv = ['assemble', '--project', str(self.old), '--player-config', str(self.source)]
        result = subprocess.run(cli + legacy_argv, capture_output=True, text=True, timeout=20)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assert_old()
        result = subprocess.run(cli + ['run', '--project', str(self.old), '--saves', str(saves),
                                       '--print-command'], capture_output=True, text=True, timeout=20)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(json.loads(result.stdout)[0], str(self.old_elf))
        for relative, original in frozen.items():
            self.assertEqual((moved / relative).read_bytes(), original)
        result = subprocess.run(cli + argv, capture_output=True, text=True, timeout=20)
        self.assertEqual(result.returncode, 0, result.stderr)

        # Malformed provenance rejects before entry/config replacement; restore
        # each isolated mutation only in this disposable scratch repository.
        provenance = self.new / 'generation.json'
        assembly = self.new / 'assembly.json'
        generation_bytes, assembly_bytes = provenance.read_bytes(), assembly.read_bytes()
        generation, assembled = json.loads(generation_bytes), json.loads(assembly_bytes)
        mutations = []
        for key, value in (('root', '/'), ('root', str(old_root.with_name('foreign'))),
                           ('bundle', str(old_root / 'generated/other')),
                           ('project', str(old_root / 'outside-project')),
                           ('executable', str(old_root / 'foreign-elf')),
                           ('executable_sha256', '0' * 64),
                           ('build_cache', str(old_root / 'target/foreign')),
                           ('logs', str(old_root / 'generated/schema2/project/logs'))):
            bad = copy.deepcopy(generation)
            bad[key] = value
            mutations.append((provenance, bad))
        for key, value in (('exe', str(old_root / 'assets/../unpacked/PAL.raw.exe')),
                           ('assets', '/foreign/assets'), ('cd_bin', '../authored.BIN')):
            bad = copy.deepcopy(generation)
            bad['target'][key] = value
            mutations.append((provenance, bad))
        for key in ('input_sha256', 'asset_sha256'):
            bad = copy.deepcopy(generation)
            bad[key][next(iter(bad[key]))] = '0' * 64
            mutations.append((provenance, bad))
            bad = copy.deepcopy(generation)
            sha = bad[key].pop(next(iter(bad[key])))
            bad[key]['/foreign/resource'] = sha
            mutations.append((provenance, bad))
        bad = copy.deepcopy(generation)
        bad['generated_sha256'] = {'../outside-project': '0' * 64}
        mutations.append((provenance, bad))
        for key, value in (('generation_sha256', '0' * 64),
                           ('executable', str(old_root / 'generated/new/project/target/debug/pal-native-generated')),
                           ('executable_sha256', '0' * 64),
                           ('bundle', str(old_root / 'generated/other')),
                           ('entry', str(old_root / 'foreign-entry'))):
            bad = copy.deepcopy(assembled)
            bad[key] = value
            mutations.append((assembly, bad))
        for key, value in (('sha256', '0' * 64),
                           ('destination', str(old_root / 'outside-seed'))):
            bad = copy.deepcopy(assembled)
            bad['player_config'][key] = value
            mutations.append((assembly, bad))
        for path, bad in mutations:
            with self.subTest(path=path, record=bad):
                module.write_json(path, bad)
                before = {p: p.read_bytes() for p in
                          (provenance, assembly, self.new / 'player-config/SETUP.DAT')}
                link = os.readlink(self.entry)
                for arguments in (argv, run_argv):
                    result = subprocess.run(cli + arguments, capture_output=True, text=True, timeout=20)
                    self.assertEqual(result.returncode, 2, result.stderr)
                    self.assertTrue(result.stderr.startswith('native-game: '), result.stderr)
                self.assertEqual(os.readlink(self.entry), link)
                self.assertFalse(saves.exists())
                for p, content in before.items():
                    self.assertEqual(p.read_bytes(), content)
                provenance.write_bytes(generation_bytes)
                assembly.write_bytes(assembly_bytes)
        for path in (self.new_elf, moved / 'assets/unpacked/PAL.raw.exe',
                     moved / 'assets/GC050.BIN', moved / 'assets/GC050.cue',
                     moved / 'assets/dos-cd/authored.dat', self.new / 'player-config/SETUP.DAT'):
            original = path.read_bytes()
            with self.subTest(tampered=path):
                path.write_bytes(b'changed authored fixture')
                result = subprocess.run(cli + argv, capture_output=True, text=True, timeout=20)
                self.assertEqual(result.returncode, 2, result.stderr)
                self.assertEqual(self.entry.resolve(), self.new_elf)
                self.assertEqual(path.read_bytes(), b'changed authored fixture')
            path.write_bytes(original)
        self.entry.unlink()
        self.entry.symlink_to(self.source)
        result = subprocess.run(cli + argv, capture_output=True, text=True, timeout=20)
        self.assertEqual(result.returncode, 2, result.stderr)
        self.assertEqual(self.entry.resolve(), self.source)
        # A candidate outside generated/ is never a publishable/run bundle.
        outside = moved / 'foreign-bundle'
        shutil.copytree(self.new, outside)
        result = subprocess.run(cli + ['run', '--project', str(outside), '--saves', str(saves),
                                       '--print-command'], capture_output=True, text=True, timeout=20)
        self.assertEqual(result.returncode, 2, result.stderr)
        self.assertFalse(saves.exists())

    def test_schema2_build_cli_publishes_independent_elf_and_assembles(self):
        tools = self.root / 'tools'
        tools.mkdir()
        for name in ('native_game.py', 'verify_assets.py', 'inspect_mz.py'):
            shutil.copyfile(Path(__file__).resolve().parents[1] / 'tools' / name, tools / name)
        # Instrument only the disposable verifier copy to reproduce mutation
        # after its first successful identity check, before build provenance.
        verifier = tools / 'verify_assets.py'
        verifier.write_text(verifier.read_text() + '\n' +
            '_verify_runtime = verify_runtime\n'
            'def verify_runtime(root, target=None):\n'
            '    result = _verify_runtime(root, target)\n'
            '    import os\n'
            '    if os.environ.pop("AUTHORED_PRECHECK_GAP", None):\n'
            '        p = Path(root) / target["exe"]\n'
            '        data = p.read_bytes(); p.write_bytes(b"x" + data[1:])\n'
            '    return result\n')
        docs = self.root / 'docs'
        docs.mkdir()
        asset = Path(self.target['assets']) / 'authored.dat'
        exe = Path(self.target['exe'])
        manifest = {'files': [{'name': asset.name, 'size': asset.stat().st_size, 'sha256': module.digest(asset)}],
                    'unpacked_files': [{'name': 'PAL.raw.exe', 'size': exe.stat().st_size, 'sha256': module.digest(exe)}]}
        # Authored identities exercise the production precheck without game bytes.
        cd = Path(self.target['cd_bin'])
        cd.with_suffix('.cue').write_text('FILE "GC050.BIN" BINARY\nTRACK 01 MODE1/2352\nINDEX 01 00:00:00\n')
        # Existing rollback fixtures must record the same authored CUE bytes.
        for previous in (self.old, self.new):
            record_path = previous / 'generation.json'
            record = json.loads(record_path.read_text())
            record['input_sha256'][str(cd.with_suffix('.cue'))] = module.digest(cd.with_suffix('.cue'))
            module.write_json(record_path, record)
        (docs / 'resource-manifest.json').write_text(json.dumps(manifest))
        (docs / 'cd-audio-manifest.json').write_text(json.dumps(
            {'bytes': cd.stat().st_size, 'sha256': module.digest(cd),
             'track_layout': [[1, 'MODE1/2352', '00:00:00']]}))
        moved_source = self.root / 'src/host/authored.rs'
        moved_source.parent.mkdir(parents=True)
        moved_source.write_text('// authored recursive source snapshot fixture\n')
        target = self.root / 'target.json'
        target.write_text(json.dumps(self.target))
        stubs = self.root / 'authored-build-tools'
        stubs.mkdir()
        stub = stubs / 'cargo'
        stub.write_text(f'#!{sys.executable}\n' +
            'import os,sys,shutil\nfrom pathlib import Path\n'
            'tool=Path(sys.argv[0]).name\na=sys.argv[1:]\n'
            'if tool != "pal-wayland-lift" and ("-V" in a or "-Vv" in a or "--modversion" in a):\n'
            ' print("authored build-tool fixture");sys.exit(0)\n'
            'if tool == "pal-wayland-lift":\n'
            ' p=Path(a[1]);(p/"src").mkdir(parents=True)\n'
            f' for name in {tuple(n for n in module.GENERATED_FILES if n != "Cargo.lock")!r}:\n'
            '  (p/name).write_text("authored generated fixture " + name)\n'
            'elif "--bin" in a:\n'
            ' p=Path(os.environ["CARGO_TARGET_DIR"])/"debug/pal-wayland-lift"\n'
            ' p.parent.mkdir(parents=True,exist_ok=True);shutil.copyfile(__file__,p);p.chmod(0o700)\n'
            'elif "generate-lockfile" in a:\n'
            ' (Path(a[a.index("--manifest-path")+1]).parent/"Cargo.lock").write_text("authored lock")\n'
            'else:\n'
            ' p=Path(os.environ["CARGO_TARGET_DIR"])/"debug/pal-wayland"\n'
            ' p.parent.mkdir(parents=True);shutil.copyfile(sys.executable,p);p.chmod(0o700)\n'
            ' if os.environ.get("AUTHORED_CHANGE_GENERATED"):\n'
            '  (Path(a[a.index("--manifest-path")+1]).parent/"src/main.rs").write_text("authored changed source")\n'
            ' if os.environ.get("AUTHORED_CHANGE_INPUT"):\n'
            f'  Path({self.target["exe"]!r}).write_bytes(b"authored changed input")\n')
        stub.chmod(0o700)
        for tool in ('rustc', 'pkg-config'):
            shutil.copyfile(stub, stubs / tool)
            (stubs / tool).chmod(0o700)
        env = os.environ.copy()
        env['PATH'] = str(stubs) + os.pathsep + env.get('PATH', '')
        env['RUSTC'] = str(stubs / 'rustc')
        env['PKG_CONFIG'] = str(stubs / 'pkg-config')
        env.pop('TMPDIR', None)  # Default publication is independent of temp configuration.
        cli = [sys.executable, str(tools / 'native_game.py')]
        output = self.root / 'generated/cli-schema2'
        original = asset.read_bytes()
        asset.write_bytes(b'x' + original[1:])
        rejected = subprocess.run(cli + ['build', '--target', str(target), '--output', str(output)],
                                  env=env, capture_output=True, text=True, timeout=20)
        self.assertEqual(rejected.returncode, 2, rejected.stderr)
        self.assertIn('hash mismatch', rejected.stderr)
        self.assertFalse(output.exists())
        self.assertFalse((self.root / 'target').exists())
        asset.write_bytes(original)
        built = subprocess.run(cli + ['build', '--target', str(target), '--output', str(output)],
                               env=env, capture_output=True, text=True, timeout=20)
        self.assertEqual(built.returncode, 0, built.stderr)
        bundle, elf, record, _ = module.ready_player(output)
        self.assertEqual(elf, output / 'bin/pal-wayland')
        self.assertFalse(elf.is_symlink())
        self.assertEqual(elf.read_bytes(), Path(sys.executable).read_bytes())
        self.assertEqual(set(record['generated_sha256']), set(module.GENERATED_FILES))
        self.assertEqual(record['source_sha256']['src/host/authored.rs'], module.digest(moved_source))
        self.assertTrue(all(Path(step['stdout']).parent == output / 'logs' for step in record['steps']))
        cache = Path(record['build_cache'])
        cached = cache / 'debug/pal-wayland'
        cached.write_bytes(b'authored invalidated disposable cache')
        module.ready_player(bundle)  # Cache contents are never run/read as player authority.
        self.assertEqual(self.assemble(project=bundle), 0)
        self.assertEqual(self.entry.resolve(), elf)
        saves = self.root / 'never-created-saves'
        result = subprocess.run(cli + ['run', '--project', str(bundle), '--saves', str(saves), '--print-command'],
                                env=env, capture_output=True, text=True, timeout=20)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(json.loads(result.stdout)[0], str(elf))
        self.assertFalse(saves.exists())
        rejected = self.root / 'generated/precheck-gap'
        original_exe = exe.read_bytes()
        gap_env = dict(env, AUTHORED_PRECHECK_GAP='1')
        failed = subprocess.run(cli + ['build', '--target', str(target), '--output', str(rejected)],
                                env=gap_env, capture_output=True, text=True, timeout=20)
        self.assertEqual(failed.returncode, 2, failed.stderr)
        self.assertIn('hash mismatch', failed.stderr)
        self.assertEqual(json.loads((rejected / 'generation.json').read_text())['status'], 'failed')
        self.assertFalse((rejected / 'bin/pal-wayland').exists())
        exe.write_bytes(original_exe)
        rejected = self.root / 'generated/generated-changed'
        env['AUTHORED_CHANGE_GENERATED'] = '1'
        before = os.readlink(self.entry)
        failed = subprocess.run(cli + ['build', '--target', str(target), '--output', str(rejected)],
                                env=env, capture_output=True, text=True, timeout=20)
        self.assertEqual(failed.returncode, 2, failed.stderr)
        self.assertIn('generated files changed during build', failed.stderr)
        self.assertEqual(json.loads((rejected / 'generation.json').read_text())['status'], 'failed')
        self.assertFalse((rejected / 'bin/pal-wayland').exists())
        self.assertEqual(os.readlink(self.entry), before)
        env.pop('AUTHORED_CHANGE_GENERATED')
        rejected = self.root / 'generated/input-changed'
        env['AUTHORED_CHANGE_INPUT'] = '1'
        before = os.readlink(self.entry)
        failed = subprocess.run(cli + ['build', '--target', str(target), '--output', str(rejected)],
                                env=env, capture_output=True, text=True, timeout=20)
        self.assertEqual(failed.returncode, 2, failed.stderr)
        self.assertEqual(json.loads((rejected / 'generation.json').read_text())['status'], 'input-changed-during-build')
        self.assertFalse((rejected / 'bin/pal-wayland').exists())
        self.assertEqual(os.readlink(self.entry), before)

    def test_schema2_exact_structure_and_provenance_reject_without_writes(self):
        self.new, self.new_elf = self.bundle('guarded', schema=2)
        provenance = self.new / 'generation.json'
        frozen = provenance.read_bytes()
        record = json.loads(frozen)
        saves = self.root / 'uncreated-saves'
        mutations = [dict(record, **{key: value}) for key, value in (
            ('executable', str(self.new / 'project/target/debug/pal-wayland')),
            ('executable', str(self.new / 'bin/pal-native-generated')),
            ('build_cache', str(self.root / 'target/foreign')),
            ('logs', str(self.new / 'project/logs')),
            ('generated_sha256', {}), ('generated_sha256', {'../outside': '0' * 64}),
            ('build_environment', {}), ('profile', module.build_profile('dev')),
            ('schema', True), ('executable_sha256', '0' * 64))]
        missing = copy.deepcopy(record)
        missing.pop('generated_sha256')
        mutations.append(missing)
        for bad in mutations:
            module.write_json(provenance, bad)
            before = {p: p.read_bytes() for p in self.new.rglob('*') if p.is_file()}
            with self.assertRaises(ValueError):
                module.ready_player(self.new)
            self.assertEqual(self.assemble(), 2)
            with contextlib.redirect_stderr(io.StringIO()):
                self.assertEqual(module.main(['run', '--project', str(self.new), '--saves', str(saves),
                                              '--print-command']), 2)
            self.assertEqual(before, {p: p.read_bytes() for p in self.new.rglob('*') if p.is_file()})
            self.assert_old()
            self.assertFalse(saves.exists())
            provenance.write_bytes(frozen)
        for relative in ('project/target/extra', 'project/src/extra.rs', 'logs/unknown.log', 'bin/other'):
            extra = self.new / relative
            extra.parent.mkdir(parents=True, exist_ok=True)
            extra.write_text('authored extra')
            self.assertEqual(self.assemble(), 2)
            self.assert_old()
            self.assertFalse((self.new / 'player-config').exists())
            extra.unlink()
            if relative.startswith('project/target/'):
                extra.parent.rmdir()
        contents = self.new_elf.read_bytes()
        self.new_elf.unlink()
        self.new_elf.symlink_to(self.old_elf)
        self.assertEqual(self.assemble(), 2)
        self.assert_old()
        self.new_elf.unlink()
        self.new_elf.write_bytes(contents)
        self.new_elf.chmod(0o700)
        self.assertEqual(self.assemble(), 0)
        self.assertEqual(self.entry.resolve(), self.new_elf)
        self.assertEqual(json.loads((self.new / 'assembly.json').read_text())['schema'], 2)
        self.assertEqual(self.assemble(project=self.old), 0)
        self.assert_old()

    def test_exact_player_layout_allowlist_rejects_name_and_schema_bypasses(self):
        for player_name in ('pal-wayland', 'pal-native-generated'):
            with self.subTest(player_name=player_name):
                bundle, elf = self.bundle('layout-' + player_name, player_name=player_name)
                path = bundle / 'generation.json'
                frozen = path.read_bytes()
                self.assertEqual(module.ready_player(bundle)[1], elf)
                self.entry.unlink()
                self.entry.symlink_to(elf.relative_to(self.root))
                self.assertEqual(module.entry_state(self.entry), os.readlink(self.entry))
                self.assertEqual(path.read_bytes(), frozen)
                record = json.loads(frozen)
                # Same hash/ELF bytes do not grant arbitrary executable paths.
                for relative in ('project/target/debug/unknown', 'project/target/debug/pal',
                                 'project/target/release/' + player_name,
                                 'target/debug/' + player_name,
                                 'project/target/debug/nested/' + player_name,
                                 'project/target/debug/../debug/' + player_name):
                    bad = dict(record, executable=str(bundle) + '/' + relative)
                    module.write_json(path, bad)
                    with self.assertRaises(ValueError):
                        module.ready_player(bundle)
                for schema in (True, '1', 0, 2):
                    module.write_json(path, dict(record, schema=schema))
                    with self.assertRaises(ValueError):
                        module.ready_player(bundle)
                path.write_bytes(frozen)
                # Do not infer a bundle from an unknown filename or layout,
                # even if a verifier stub were to return that destination.
                for relative in ('project/target/debug/unknown',
                                 'project/target/release/' + player_name,
                                 'project/target/debug/nested/' + player_name):
                    self.entry.unlink()
                    self.entry.symlink_to((bundle / relative).relative_to(self.root))
                    with mock.patch.object(module, 'ready_player') as verify:
                        with self.assertRaisesRegex(ValueError, 'foreign player symlink'):
                            module.entry_state(self.entry)
                    verify.assert_not_called()
                # No fallback to a hash-identical allowed filename.
                alternate = elf.with_name('pal-wayland' if player_name == 'pal-native-generated'
                                          else 'pal-native-generated')
                shutil.copyfile(elf, alternate)
                alternate.chmod(0o700)
                self.entry.unlink()
                self.entry.symlink_to(alternate.relative_to(self.root))
                with self.assertRaisesRegex(ValueError, 'foreign player symlink'):
                    module.entry_state(self.entry)
                elf.unlink()
                with self.assertRaisesRegex(ValueError, 'missing native ELF'):
                    module.ready_player(bundle)
                self.assertEqual(path.read_bytes(), frozen)

    def test_missing_wrong_size_and_symlink_seeds_retain_entry(self):
        alias = self.root / "seed-link"
        alias.symlink_to(self.source)
        for source in (self.root / "missing", alias):
            with self.subTest(source=source):
                self.assertEqual(self.assemble(player_config=source), 2)
                self.assert_old()
        for size in (0, 19, 21):
            self.source.write_bytes(b"a" * size)
            self.assertEqual(self.assemble(), 2)
            self.assert_old()
        self.assertFalse((self.new / "player-config").exists())

    def test_symlink_seed_ancestor_rejected(self):
        alias = self.root / "alias"
        alias.symlink_to(self.root, target_is_directory=True)
        self.assertEqual(self.assemble(player_config=alias / self.source.name), 2)
        self.assert_old()

    def test_existing_bundle_config_never_overwritten(self):
        seed = self.new / "player-config/SETUP.DAT"
        seed.parent.mkdir()
        original = b"existing user settings"
        seed.write_bytes(original)
        self.assertEqual(self.assemble(), 2)
        self.assertEqual(seed.read_bytes(), original)
        self.assert_old()

    def test_bundle_config_symlink_never_followed(self):
        config = self.new / "player-config"
        config.symlink_to(self.root, target_is_directory=True)
        self.assertEqual(self.assemble(), 2)
        self.assertFalse((self.root / "SETUP.DAT").exists())
        self.assert_old()

    def test_foreign_link_and_regular_entry_never_overwritten(self):
        self.entry.unlink()
        self.entry.symlink_to(self.source)
        self.assertEqual(self.assemble(), 2)
        self.assertEqual(self.entry.resolve(), self.source)
        self.entry.unlink()
        self.entry.write_text("user file")
        self.assertEqual(self.assemble(), 2)
        self.assertEqual(self.entry.read_text(), "user file")
        self.assertFalse((self.new / "player-config").exists())

    def test_arbitrary_entry_refused(self):
        self.assertEqual(self.assemble(entry=self.root / "other"), 2)
        self.assertFalse((self.root / "other").exists())
        self.assert_old()

    def test_unready_and_changed_elf_refused(self):
        provenance = self.new / "generation.json"
        record = json.loads(provenance.read_text())
        record['status'] = 'failed'
        module.write_json(provenance, record)
        self.assertEqual(self.assemble(), 2)
        self.assert_old()
        record['status'] = 'ready'
        module.write_json(provenance, record)
        self.new_elf.write_bytes(b"\x7fELFchanged authored fixture")
        self.assertEqual(self.assemble(), 2)
        self.assert_old()

    def test_changed_original_resource_refused(self):
        Path(self.target['exe']).write_bytes(b"modified input")
        self.assertEqual(self.assemble(), 2)
        self.assert_old()
        self.assertFalse((self.new / "player-config").exists())

    def test_changed_asset_and_nonstandard_resource_layout_refused(self):
        asset = Path(self.target['assets']) / 'authored.dat'
        asset.write_bytes(b'modified resource')
        self.assertEqual(self.assemble(), 2)
        self.assert_old()
        asset.write_bytes(b'authored resource')
        alternative = self.root / 'alternative.exe'
        alternative.write_bytes(b'original authored input')
        path = self.new / 'generation.json'
        record = json.loads(path.read_text())
        record['target']['exe'] = str(alternative)
        module.write_json(path, record)
        self.assertEqual(self.assemble(), 2)
        self.assert_old()
        self.assertFalse((self.new / 'player-config').exists())

    def test_schema2_metadata_failure_leaves_bundle_retryable(self):
        self.new, self.new_elf = self.bundle("metadata-failure", schema=2)
        original_link = module.os.link

        def fail_metadata(source, destination):
            if Path(destination).name == "assembly.json":
                raise OSError("authored metadata publication failure")
            return original_link(source, destination)

        with mock.patch.object(module.os, "link", fail_metadata):
            self.assertEqual(self.assemble(), 2)
        self.assert_old()
        self.assertFalse((self.new / "player-config").exists())
        self.assertFalse((self.new / "assembly.json").exists())
        module.ready_player(self.new)
        self.assertEqual(self.assemble(), 0)
        self.assertEqual(self.entry.resolve(), self.new_elf)

    def test_publish_failure_retains_old_entry_and_retry_is_safe(self):
        with mock.patch.object(module.os, "replace", side_effect=OSError("authored publication failure")):
            self.assertEqual(self.assemble(), 2)
        self.assert_old()
        seed = self.new / "player-config/SETUP.DAT"
        self.assertEqual(seed.read_bytes(), self.source.read_bytes())
        self.assertEqual(self.assemble(), 0)
        self.assertEqual(self.entry.resolve(), self.new_elf)

    def test_modified_assembled_seed_not_overwritten(self):
        self.assertEqual(self.assemble(), 0)
        seed = self.new / "player-config/SETUP.DAT"
        seed.write_bytes(b"user modified setting")
        self.assertEqual(self.assemble(), 2)
        self.assertEqual(seed.read_bytes(), b"user modified setting")
        self.assertEqual(self.entry.resolve(), self.new_elf)

    def test_source_change_before_publication_retains_old_entry(self):
        original = module.digest
        source_calls = 0

        def changed_digest(path):
            nonlocal source_calls
            if Path(path) == self.source:
                source_calls += 1
                self.source.write_bytes(b'x' * 20)
            return original(path)

        with mock.patch.object(module, 'digest', side_effect=changed_digest):
            self.assertEqual(self.assemble(), 2)
        self.assertGreater(source_calls, 0)
        self.assert_old()
        self.assertEqual((self.new / 'player-config/SETUP.DAT').read_bytes(), bytes(range(20)))

    def test_entry_change_before_publication_does_not_overwrite_new_user_file(self):
        original = module.entry_state
        calls = 0

        def changed_entry(path):
            nonlocal calls
            calls += 1
            if calls == 2:
                self.entry.unlink()
                self.entry.write_text('concurrent user file')
            return original(path)

        with mock.patch.object(module, 'entry_state', side_effect=changed_entry):
            self.assertEqual(self.assemble(), 2)
        self.assertEqual(self.entry.read_text(), 'concurrent user file')

    def test_new_entry_supported(self):
        self.entry.unlink()
        self.assertEqual(self.assemble(), 0)
        self.assertEqual(self.entry.resolve(), self.new_elf)


if __name__ == "__main__":
    unittest.main()
