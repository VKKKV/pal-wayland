#!/usr/bin/env python3
# SPDX-License-Identifier: GPL-3.0-or-later
"""Bounded reproducible AOT build/execution iterations. No CPU fallback or GUI.

The target manifest contains confirmed roots/tables, never embedded guest bytes.
Each iteration creates a new ignored/scratch project and saves full real output.
Unknown runtime targets may become observed roots for the next BUILD; they are
never decoded or executed as instructions inside the native runtime.
"""
import argparse
import json
import os
from pathlib import Path
import re
import shutil
import signal
import subprocess
import sys
import time
from typing import Any

ROOT = Path(__file__).resolve().parents[1]


def build_environment(project=None):
    env = os.environ.copy()
    env.pop('CARGO_BUILD_TARGET', None)
    env['CARGO_TARGET_DIR'] = str((project or ROOT) / 'target')
    # Development exploration must retain its checked runtime contract too.
    env['CARGO_PROFILE_DEV_OVERFLOW_CHECKS'] = 'true'
    env['CARGO_PROFILE_DEV_DEBUG_ASSERTIONS'] = 'true'
    return env


def signal_group(proc, sig):
    # Only our start_new_session child: PID is the exclusive owned PGID.
    try:
        os.killpg(proc.pid, sig)
    except ProcessLookupError:
        pass


def invoke(args, cwd, timeout, *, env=None) -> dict[str, Any]:
    proc = subprocess.Popen(args, cwd=cwd, env=env, stdout=subprocess.PIPE,
                            stderr=subprocess.PIPE, text=True, start_new_session=True)
    timed_out = False
    try:
        try:
            stdout, stderr = proc.communicate(timeout=timeout)
        except subprocess.TimeoutExpired:
            timed_out = True
            signal_group(proc, signal.SIGTERM)
            try:
                proc.communicate(timeout=0.2)
            except subprocess.TimeoutExpired:
                pass
            # Pipe EOF/leader exit does not prove the descendants stopped.
            signal_group(proc, signal.SIGKILL)
            stdout, stderr = proc.communicate(timeout=1)
        return dict(exit_code=124 if timed_out else proc.returncode,
                    stdout=stdout, stderr=stderr, timed_out=timed_out, command=args)
    finally:
        # Also clean up on interruption or a successful leader exit. Never
        # look up a leader's current PGID (which could be a shared group).
        signal_group(proc, signal.SIGKILL)
        for pipe in (proc.stdout, proc.stderr):
            if pipe is not None:
                pipe.close()
        proc.wait(timeout=1)


def run_command(target, executable, saves, memory, args, *, frame=None):
    """Headless development replay: fixed *fractional* DOS centiseconds, not time-of-day."""
    command = [str(executable), str(ROOT / target['exe']),
               '--assets', str(ROOT / target['assets']), '--saves', str(saves),
               '--cd-bin', str(ROOT / target['cd_bin']),
               '--ram-top', target.get('ram_top', 'a000'), '--irq', 'enabled',
               '--clock-centiseconds', '0', '--window', 'no', '--audio-output', 'off',
               '--max-steps', str(args.max_steps), '--memory', str(memory)]
    if target.get('cd_header') == 'legacy13':
        command += ['--cd-header', 'legacy13']
    for event in args.key_event:
        command += ['--key-event', event]
    if args.virtual_step_ns is not None:
        if not 0 < args.virtual_step_ns <= (1 << 64) - 1:
            raise ValueError('virtual-step-ns must be a positive u64')
        command += ['--virtual-step-ns', str(args.virtual_step_ns)]
    if frame is not None:
        command += ['--frame-ppm', str(frame)]
    return command


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--target', type=Path, default=ROOT/'docs/native-target.json')
    parser.add_argument('--iterations', type=int, default=1)
    parser.add_argument('--max-steps', type=int, default=5_000_000)
    parser.add_argument('--build-timeout', type=int, default=240)
    parser.add_argument('--run-timeout', type=int, default=60)
    parser.add_argument('--frame', action='store_true')
    parser.add_argument('--key-event',action='append',default=[])
    parser.add_argument('--virtual-step-ns',type=int)
    parser.add_argument('--output', type=Path)
    args = parser.parse_args(argv)
    if args.virtual_step_ns is not None and not 0 < args.virtual_step_ns <= (1 << 64) - 1:
        parser.error('virtual-step-ns must be a positive u64')
    if not 1 <= args.iterations <= 30 or args.max_steps <= 0:
        parser.error('iterations must be 1..30, max-steps must be positive')
    for name in ('cargo', 'bwrap'):
        if not shutil.which(name):
            parser.error(f'required command unavailable: {name}')
    scratch = Path(os.environ.get('TMPDIR', str(ROOT/'target'))).resolve()
    target = json.loads(args.target.read_text())
    seeds = list(target['seeds'])
    tables = list(target['tables'])
    for token in seeds + target.get('mutable_immediates', []):
        if not re.fullmatch('[0-9a-fA-F]{4}:[0-9a-fA-F]{4}', token):
            parser.error(f'invalid exact address: {token!r}')
    for token in tables:
        if not re.fullmatch('[0-9a-fA-F]{4}:[0-9a-fA-F]{4}:[0-9]+', token):
            parser.error(f'invalid exact table: {token!r}')
    output = args.output or scratch/f'pal-native-dev-{time.time_ns()}'
    output = output.resolve()
    allowed = [scratch, ROOT/'generated', ROOT/'target']
    if not any(output.is_relative_to(p) and output != p for p in allowed):
        parser.error('output must be a NEW child directory in scratch/generated/target')
    sandbox = ['bwrap', '--unshare-all', '--die-with-parent', '--new-session',
               '--ro-bind', '/', '/', '--bind', str(scratch), str(scratch),
               '--proc', '/proc', '--dev', '/dev', '--']
    health = invoke(sandbox+['/usr/bin/true'], ROOT, 15)
    if health['exit_code']:
        print(json.dumps(health, indent=2)); return 1
    # No existing files are overwritten and input assets remain read-only.
    output.mkdir()
    saves = scratch/f'pal-native-saves-{time.time_ns()}'
    saves.mkdir(mode=0o700)
    built = invoke(['cargo','build','--offline','--locked','--profile','dev','--bin','pal-wayland-lift'],
                   ROOT, args.build_timeout, env=build_environment())
    (output/'tool-build.json').write_text(json.dumps(built,indent=2))
    if built['exit_code']:
        print(built['stderr'],file=sys.stderr); return 1
    history = []
    for index in range(args.iterations):
        project = output/f'iteration-{index+1}'
        generate = [str(ROOT/'target/debug/pal-wayland-lift'),
                    str(ROOT/target['exe']),str(project),'--fpu-macros','borland']
        for flag, tokens in [('--seed',seeds),('--near-table',tables),
                             ('--mutable-immediate',target.get('mutable_immediates',[]))]:
            for token in tokens: generate += [flag,token]
        result: dict[str, Any] = dict(seeds=seeds[:], tables=tables, directory=str(project))
        result['generation'] = invoke(generate, ROOT, 60)
        if result['generation']['exit_code']:
            history.append(result); break
        result['build'] = invoke(['cargo','build','--offline','--profile','dev','--manifest-path',
                                  str(project/'Cargo.toml')],ROOT,args.build_timeout,
                                 env=build_environment(project))
        if result['build']['exit_code']:
            history.append(result); break
        run = sandbox + run_command(
            target, project / 'target/debug/pal-wayland', saves,
            scratch / f'pal-dev-{time.time_ns()}.mem', args,
            frame=scratch / f'pal-frame-{time.time_ns()}.ppm' if args.frame else None)
        result['execution'] = invoke(run,ROOT,args.run_timeout)
        history.append(result)
        # Observations are developmental roots only, not a coverage proof.
        match = re.search(r'pal-wayland: ([0-9a-f]{4}:[0-9a-f]{4}).*unknown static target',
                          result['execution']['stderr'])
        print(f'{project}: '+result['execution']['stdout'].strip())
        print(result['execution']['stderr'].splitlines()[-1:] or ['no stderr'])
        if (result['execution']['timed_out'] or result['execution']['exit_code'] != 1
                or not match or match[1] in seeds): break
        seeds.append(match[1])
    (output/'report.json').write_text(json.dumps(history,indent=2))
    print(f'report: {output}/report.json')
    print('scope: exploratory native execution, NOT playable-game certification')
    # Even successful guest exit is not a playable-game acceptance signal.
    last = history[-1] if history else {}
    return 0 if last.get('execution',{}).get('exit_code') == 0 else 1


if __name__ == '__main__':
    sys.exit(main())
