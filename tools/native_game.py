#!/usr/bin/env python3
# SPDX-License-Identifier: GPL-3.0-or-later
"""Build persistent native PAL artifacts and explicitly launch them (stdlib only)."""
import argparse
import hashlib
import json
import os
from pathlib import Path
import re
import signal
import selectors
import shutil
import subprocess
import sys
import tempfile
import time
import stat
import uuid

from verify_assets import verify_runtime

ROOT = Path(__file__).resolve().parents[1]
ADDRESS = re.compile(r"[0-9a-fA-F]{4}:[0-9a-fA-F]{4}")
TABLE = re.compile(r"[0-9a-fA-F]{4}:[0-9a-fA-F]{4}:[0-9]+")
GENERATED_FILES = ("Cargo.toml", "Cargo.lock", "src/main.rs", "src/translated.rs", "source-map.txt", "SCOPE.txt")
PLAYER_PATH = Path("project/target/debug/pal-wayland")  # Frozen schema 1 layout.
SCHEMA2_PLAYER_PATH = Path("bin/pal-wayland")
BUILD_LABELS = ("tool-build", "generate", "lock", "native-build")
IDENTITY_LABELS = ("identity-rustc", "identity-cargo", "identity-libpipewire-0.3",
                   "identity-libspa-0.2", "identity-wayland-client", "identity-xkbcommon", "identity-vulkan")
LOG_FILES = frozenset(f"{label}.{stream}.log" for label in BUILD_LABELS + IDENTITY_LABELS
                      for stream in ("stdout", "stderr"))
# Frozen schema 1 bundles retain their original filename and provenance bytes.
# This is an exact layout allowlist, not permission to select arbitrary ELFs.
LEGACY_SCHEMA1_PLAYER_PATHS = frozenset({Path("project/target/debug/pal-native-generated")})


def digest(path):
    with Path(path).open("rb") as stream:
        return hashlib.file_digest(stream, "sha256").hexdigest()


def target_path(value):
    if not isinstance(value, str) or not value or "\x00" in value:
        raise ValueError("target paths must be nonempty strings without NUL")
    path = Path(value)
    return (ROOT / path).resolve()


def validate_target(target):
    if not isinstance(target, dict):
        raise ValueError("target must be a JSON object")
    for key in ("exe", "assets", "cd_bin"):
        path = target_path(target.get(key))
        if not (path.is_dir() if key == "assets" else path.is_file()):
            raise ValueError(f"missing {key}: {path}")
    cue = target_path(target["cd_bin"]).with_suffix(".cue")
    if not cue.is_file():
        raise ValueError(f"missing CD cue: {cue}")
    for key in ("seeds", "tables", "mutable_immediates"):
        tokens = target.get(key, [])
        if not isinstance(tokens, list):
            raise ValueError(f"{key} must be a list")
        for token in tokens:
            pattern = TABLE if key == "tables" else ADDRESS
            if not isinstance(token, str) or not pattern.fullmatch(token):
                raise ValueError(f"invalid exact {key} token: {token!r}")
            if key == "tables":
                _, offset, count = token.split(":")
                count = int(count)
                if not 1 <= count <= 32768 or int(offset, 16) + 2 * count > 65536:
                    raise ValueError(f"table outside segment/count range: {token!r}")
    ram = target.get("ram_top", "a000")
    if not isinstance(ram, str) or not re.fullmatch(r"[0-9a-fA-F]{4}", ram):
        raise ValueError(f"invalid exact ram_top: {ram!r}")
    if not 0x9f00 <= int(ram, 16) <= 0xa000:
        raise ValueError("ram_top must be between 9f00 and a000")
    if target.get("cd_header") not in (None, "legacy13"):
        raise ValueError("cd_header must be absent or exactly legacy13")
    return target


def generation_command(target, project, generator):
    command = [str(generator), str(target_path(target["exe"])), str(project),
               "--fpu-macros", "borland"]
    for flag, key in (("--seed", "seeds"), ("--near-table", "tables"),
                      ("--mutable-immediate", "mutable_immediates")):
        for token in target.get(key, []):
            command.extend((flag, token))
    return command


def overlaps(first, second):
    return first == second or first.is_relative_to(second) or second.is_relative_to(first)


def validate_saves(saves, assets):
    saves, assets = Path(saves).resolve(), Path(assets).resolve()
    if overlaps(saves, assets):
        raise ValueError("saves and assets must be disjoint directories (no parent/child overlap)")
    if saves.exists() and not saves.is_dir():
        raise ValueError(f"saves is not a directory: {saves}")
    # The COW root must not contain symlinks back into original resources.
    if saves.exists() and any(p.is_symlink() for p in saves.rglob("*")):
        raise ValueError("saves must not contain symlinks")
    return saves


def run_command(target, executable, saves, *, window="no", headless=False,
                audio_output="off", sound_blaster="no", max_steps=None,
                virtual_step_ns=None):
    if headless and window == "yes":
        raise ValueError("--headless conflicts with --window yes")
    if virtual_step_ns is not None and audio_output != "off":
        raise ValueError("--virtual-step-ns requires --audio-output off")
    for name, value in (("max-steps", max_steps), ("virtual-step-ns", virtual_step_ns)):
        if value is not None and not 0 < value <= (1 << 64) - 1:
            raise ValueError(f"{name} must be a positive u64")
    saves = validate_saves(saves, target_path(target["assets"]))
    command = [str(executable), str(target_path(target["exe"])),
               "--assets", str(target_path(target["assets"])), "--saves", str(saves),
               "--cd-bin", str(target_path(target["cd_bin"])),
               "--ram-top", target.get("ram_top", "a000"), "--irq", "enabled",
               "--window", "no" if headless else window,
               "--audio-output", audio_output, "--sound-blaster", sound_blaster]
    if target.get("cd_header") == "legacy13":
        command.extend(("--cd-header", "legacy13"))
    if sound_blaster == "yes":
        command.extend(("--opl", "absent"))
    if max_steps is not None:
        command.extend(("--max-steps", str(max_steps)))
    if virtual_step_ns is not None:
        command.extend(("--virtual-step-ns", str(virtual_step_ns)))
    return command


def build_profile(name):
    """Named tool profiles share Cargo dev settings; schema 2 publishes a separate ELF."""
    if name not in ("player", "dev"):
        raise ValueError(f"unknown build profile: {name!r}")
    return {"name": name, "cargo_profile": "dev", "opt_level": 3 if name == "player" else 1,
            "debug": 0, "incremental": False, "overflow_checks": True,
            "debug_assertions": True, "codegen_units": 16, "lto": False,
            "panic": "unwind", "strip": "none", "rpath": False}


def build_environment(project=None, *, profile="dev", cache=None):
    settings = build_profile(profile)
    env = os.environ.copy()
    # Never let another build's inherited target directory select an unrelated ELF.
    env.pop("CARGO_BUILD_TARGET", None)
    env["CARGO_TARGET_DIR"] = str(cache if cache is not None else project / "target" if project else ROOT / "target")
    if project:
        # rustc flags take precedence over Cargo profile settings. Reject rather
        # than silently discard user flags or claim checks/optimization we lack.
        flags = [key for key, value in env.items() if value and
                 (key in ("RUSTFLAGS", "CARGO_ENCODED_RUSTFLAGS", "CARGO_BUILD_RUSTFLAGS")
                  or (key.startswith("CARGO_TARGET_") and key.endswith("_RUSTFLAGS")))]
        if flags:
            raise ValueError("reproducible build requires unset rustc flags: " + ", ".join(sorted(flags)))
        for key in list(env):
            if key.startswith("CARGO_PROFILE_"):
                del env[key]
        for key, value in settings.items():
            if key not in ("name", "cargo_profile"):
                env["CARGO_PROFILE_DEV_" + key.upper()] = str(value).lower()
        env["CARGO_INCREMENTAL"] = "0"
    return env


def recorded_environment(env):
    """Only reproducibility settings, never the caller's full environment/secrets."""
    return {key: value for key, value in sorted(env.items()) if
            key.startswith("CARGO_PROFILE_") or key in ("CARGO_TARGET_DIR", "CARGO_INCREMENTAL")}


def signal_group(proc, sig):
    """Only for our start_new_session child: its PID is its exclusive PGID."""
    try:
        os.killpg(proc.pid, sig)
    except ProcessLookupError:
        pass  # The entire group may already have exited.


def terminate(proc):
    signal_group(proc, signal.SIGTERM)
    try:
        proc.wait(timeout=0.2)
    except subprocess.TimeoutExpired:
        pass
    finally:
        # Waiting for the leader says nothing about descendants. Always escalate
        # the owned group, even when the leader promptly accepted SIGTERM.
        signal_group(proc, signal.SIGKILL)
        proc.wait()  # Reap our child, not grandchildren we do not own.


def capture_bounded(proc, stdout, stderr, deadline, limit):
    """Drain nonblocking pipes fairly; persist at most limit bytes per stream."""
    with selectors.DefaultSelector() as selector:
        for pipe, log in ((proc.stdout, stdout), (proc.stderr, stderr)):
            os.set_blocking(pipe.fileno(), False)
            selector.register(pipe, selectors.EVENT_READ, [log, 0])
        try:
            while selector.get_map():
                remaining = deadline - time.monotonic()
                if remaining <= 0:
                    raise subprocess.TimeoutExpired(proc.args, 0)
                for key, _ in selector.select(remaining):
                    log, count = key.data
                    # One extra byte distinguishes exact-budget output from
                    # excess, without allocating or reading an unbounded chunk.
                    chunk = os.read(key.fd, min(8192, limit - count + 1))
                    if not chunk:
                        selector.unregister(key.fileobj)
                        continue
                    kept = chunk[:limit - count]
                    log.write(kept)
                    key.data[1] += len(kept)
                    if len(chunk) > len(kept):
                        return None, True
            remaining = deadline - time.monotonic()
            if remaining <= 0:
                raise subprocess.TimeoutExpired(proc.args, 0)
            return proc.wait(timeout=remaining), False
        finally:
            proc.stdout.close()
            proc.stderr.close()


def invoke(command, bundle, label, timeout, env, *, output_limit_bytes=None):
    if output_limit_bytes is not None and (type(output_limit_bytes) is not int or output_limit_bytes <= 0):
        raise ValueError("output_limit_bytes must be a positive integer")
    start = time.monotonic()
    stdout_path, stderr_path = bundle / f"{label}.stdout.log", bundle / f"{label}.stderr.log"
    with stdout_path.open("xb") as stdout, stderr_path.open("xb") as stderr:
        bounded = output_limit_bytes is not None
        # Ordinary builds retain their full file-backed Cargo/generator logs.
        proc = subprocess.Popen(command, cwd=ROOT, env=env,
                                stdout=subprocess.PIPE if bounded else stdout,
                                stderr=subprocess.PIPE if bounded else stderr,
                                start_new_session=True)
        timed_out = output_limit_exceeded = False
        try:
            if bounded:
                code, output_limit_exceeded = capture_bounded(
                    proc, stdout, stderr, start + timeout, output_limit_bytes)
                if output_limit_exceeded:
                    terminate(proc)
                    code = 125
            else:
                code = proc.wait(timeout=timeout)
        except subprocess.TimeoutExpired:
            timed_out = True
            terminate(proc)
            code = 124
        except BaseException:
            terminate(proc)
            raise
        finally:
            # Also stop background descendants on successful leader exit.
            signal_group(proc, signal.SIGKILL)
            proc.wait()
    return {"command": command, "exit_code": code, "timed_out": timed_out,
            "output_limit_bytes": output_limit_bytes,
            "output_limit_exceeded": output_limit_exceeded,
            "logs_complete": not (timed_out or output_limit_exceeded),
            "elapsed_seconds": time.monotonic() - start,
            "stdout": str(stdout_path), "stderr": str(stderr_path)}


def log_tail(path):
    """Read at most 8000 bytes; retain the complete build log on disk."""
    with Path(path).open("rb") as stream:
        stream.seek(max(0, os.fstat(stream.fileno()).st_size - 8000))
        return stream.read(8000).decode("utf-8", errors="replace")


def write_json(path, content):
    path.write_text(json.dumps(content, ensure_ascii=False, indent=2) + "\n")


IDENTITY_OUTPUT_LIMIT_BYTES = 64 * 1024  # Per stdout/stderr log.
IDENTITY_READ_LIMIT_BYTES = IDENTITY_OUTPUT_LIMIT_BYTES + 1  # Includes excess sentinel.
IDENTITY_VERSION_LIMIT_BYTES = 16 * 1024
IDENTITY_SUMMARY_LIMIT_BYTES = 4 * 1024


def bounded_text(text, limit):
    """Bound serialized UTF-8 text even when decoding replaced invalid bytes."""
    return text.encode("utf-8")[:limit].decode("utf-8", errors="ignore")


def read_identity_log(path):
    with Path(path).open("rb") as stream:
        data = stream.read(IDENTITY_READ_LIMIT_BYTES)
    return data[:IDENTITY_OUTPUT_LIMIT_BYTES].decode("utf-8", errors="replace").strip(), len(data) > IDENTITY_OUTPUT_LIMIT_BYTES


def identity_probe(tool, arguments, bundle, label, timeout, env):
    """Bound execution, both log streams, log reads and provenance fields."""
    executable = shutil.which(tool, path=env.get("PATH", os.defpath))
    command = [executable or tool, *arguments]
    budgets = {"output_limit_bytes": IDENTITY_OUTPUT_LIMIT_BYTES,
               "read_limit_bytes": IDENTITY_READ_LIMIT_BYTES,
               "version_limit_bytes": IDENTITY_VERSION_LIMIT_BYTES,
               "summary_limit_bytes": IDENTITY_SUMMARY_LIMIT_BYTES}
    if executable is None:
        return {"command": command, "executable": None, "status": "unavailable",
                "exit_code": None, "timed_out": False, "version": None, **budgets,
                "error": bounded_text(f"executable not found: {tool}", IDENTITY_SUMMARY_LIMIT_BYTES)}
    try:
        result = invoke(command, bundle, label, min(timeout, 10), env,
                        output_limit_bytes=IDENTITY_OUTPUT_LIMIT_BYTES)
    except OSError as error:
        return {"command": command, "executable": executable, "status": "failed",
                "exit_code": None, "timed_out": False, "version": None, **budgets,
                "error": bounded_text(str(error), IDENTITY_SUMMARY_LIMIT_BYTES)}
    stdout, stdout_excess = read_identity_log(result["stdout"])
    stderr, stderr_excess = read_identity_log(result["stderr"])
    output_excess = result.get("output_limit_exceeded", False) or stdout_excess or stderr_excess
    version_excess = len(stdout.encode("utf-8")) > IDENTITY_VERSION_LIMIT_BYTES
    known = result["exit_code"] == 0 and bool(stdout) and not (output_excess or version_excess or result["timed_out"])
    result.update(budgets)
    result.update(executable=executable, status="known" if known else "failed",
                  output_limit_exceeded=output_excess, version_limit_exceeded=version_excess,
                  logs_complete=result.get("logs_complete", True) and not output_excess,
                  version=stdout if known else None)
    if not known:
        if output_excess:
            error = "identity output_limit_exceeded; preserved logs are bounded prefixes"
        elif version_excess:
            error = "identity version_limit_exceeded; version rejected (see complete bounded logs)"
        elif result["timed_out"]:
            error = "identity query timed out" + (": " + stderr if stderr else "")
        else:
            error = stderr or "identity query failed or returned no version"
        result["error"] = bounded_text(error, IDENTITY_SUMMARY_LIMIT_BYTES)
        result["summary_limit_exceeded"] = len(error.encode("utf-8")) > IDENTITY_SUMMARY_LIMIT_BYTES
    return result


def build_identity(bundle, env, timeout, features):
    """Diagnostic evidence, not a bit-reproducibility or runtime-availability claim."""
    timeout = min(timeout, 10)
    rustc = identity_probe(env.get("RUSTC") or "rustc", ["-Vv"], bundle,
                           "identity-rustc", timeout, env)
    cargo = identity_probe("cargo", ["-V"], bundle, "identity-cargo", timeout, env)
    rustc_fields: dict[str, str] = {}
    for line in (rustc["version"] or "").splitlines():
        if ": " in line:
            key, value = line.split(": ", 1)
            rustc_fields[key] = value
    rustc.update(host_target=rustc_fields.get("host"), release=rustc_fields.get("release"))
    packages = [("libpipewire-0.3", True), ("libspa-0.2", True)]
    if "window" in features:
        # The window backend dlopens these runtime libraries. pkg-config
        # metadata is useful evidence, but may legitimately be absent.
        packages += [("wayland-client", False), ("xkbcommon", False), ("vulkan", False)]
    libraries = {}
    for package, required in packages:
        probe = identity_probe(env.get("PKG_CONFIG") or "pkg-config",
                               ["--modversion", package], bundle,
                               "identity-" + package, timeout, env)
        probe["required_at_build"] = required
        libraries[package] = probe
    return {"rustc": rustc, "cargo": cargo, "system_libraries": libraries,
            "cargo_build_target": env.get("CARGO_BUILD_TARGET"),
            "scope": "build-start tool and pkg-config metadata; missing versions are explicit; not bit-reproducibility"}


def source_snapshot():
    paths = [ROOT / "Cargo.toml", ROOT / "Cargo.lock", Path(__file__).resolve(),
             ROOT / "examples" / "config.toml",
             ROOT / "tools" / "verify_assets.py", ROOT / "tools" / "inspect_mz.py",
             ROOT / "docs" / "resource-manifest.json", ROOT / "docs" / "cd-audio-manifest.json"]
    paths += sorted((ROOT / "src").rglob("*.rs"))
    paths += sorted((ROOT / "templates").glob("*"))
    return {str(p.relative_to(ROOT)): digest(p) for p in paths if p.is_file()}


def build(args):
    target = validate_target(json.loads(args.target.resolve().read_text()))
    relocated_target({"root": str(ROOT), "target": target})
    output = plain_path(args.output or ROOT / "generated" / f"native-{uuid.uuid4().hex}")
    generated = plain_path(ROOT / "generated")
    if output.parent != generated:
        raise ValueError("build output must be a NEW child of repository generated/")
    if output.exists() or output.is_symlink():
        raise ValueError(f"refusing to overwrite existing build: {output}")
    if not 0 < args.build_timeout <= 1800:
        raise ValueError("build-timeout must be 1..1800 seconds per child")
    profile = build_profile(args.profile)
    cache = plain_path(ROOT / "target/native" / output.name)
    if cache.exists() or cache.is_symlink():
        raise ValueError(f"refusing to reuse existing build cache: {cache}")
    native_env = build_environment(output / "project", profile=args.profile, cache=cache)
    verify_runtime(ROOT, target)  # Supported identity, before any build output or Cargo child.
    output.parent.mkdir(parents=True, exist_ok=True)
    output.mkdir(mode=0o700)
    project = output / "project"
    logs = output / "logs"
    logs.mkdir(mode=0o700)
    record_path = output / "generation.json"
    record = {"schema": 2, "status": "building", "root": str(ROOT),
              "project": str(project), "bundle": str(output), "target": target,
              "build_cache": str(cache), "logs": str(logs),
              "target_manifest": str(args.target.resolve()),
              "target_manifest_sha256": digest(args.target.resolve()),
              "source_sha256": source_snapshot(), "steps": [],
              "profile": profile,
              "build_environment": recorded_environment(native_env),
              "features": ["window", "profiling"] if getattr(args, "profiling", False) else ["window"], "spawned_game": False,
              "scope": "local copyrighted derivative; no runtime decoder; unknown targets fail closed"}
    exe = target_path(target["exe"])
    cd = target_path(target["cd_bin"])
    record["input_sha256"] = {str(p): digest(p) for p in (exe, cd, cd.with_suffix(".cue"))}
    record["asset_sha256"] = {str(p): digest(p) for p in sorted(target_path(target["assets"]).rglob("*")) if p.is_file()}
    write_json(record_path, record)
    try:
        record["build_identity"] = build_identity(logs, native_env, args.build_timeout, record["features"])
    except (OSError, KeyboardInterrupt) as error:
        record.update(status="failed", error=str(error))
        write_json(record_path, record)
        raise
    write_json(record_path, record)
    generator = ROOT / "target/debug/pal-wayland-lift"
    steps = [("tool-build", ["cargo", "build", "--offline", "--locked", "--bin", "pal-wayland-lift"], build_environment()),
             ("generate", generation_command(target, project, generator), build_environment()),
             ("lock", ["cargo", "generate-lockfile", "--offline", "--manifest-path", str(project / "Cargo.toml")], native_env),
             ("native-build", ["cargo", "build", "--offline", "--locked", "--profile", "dev", "--features", ",".join(record["features"]), "--manifest-path", str(project / "Cargo.toml")], native_env)]
    print(f"build_bundle: {output}", flush=True)
    for label, command, env in steps:
        print(f"{label}: logs {logs}/{label}.*.log", file=sys.stderr, flush=True)
        try:
            result = invoke(command, logs, label, args.build_timeout, env)
        except (OSError, KeyboardInterrupt) as error:
            record.update(status="failed", error=str(error))
            write_json(record_path, record)
            raise
        result["environment"] = recorded_environment(env)
        record["steps"].append(result)
        if result["exit_code"]:
            record["status"] = "failed"
            write_json(record_path, record)
            print(f"build failed; preserved logs and provenance: {record_path}", file=sys.stderr)
            print(log_tail(result["stderr"]), file=sys.stderr)
            return result["exit_code"]
        if label == "lock":
            try:
                project_inventory(project)
                record["generated_sha256"] = {name: digest(project / name) for name in GENERATED_FILES}
            except (ValueError, OSError, KeyboardInterrupt) as error:
                record.update(status="failed", error=str(error))
                write_json(record_path, record)
                raise
        write_json(record_path, record)
    if source_snapshot() != record["source_sha256"]:
        record["status"] = "source-changed-during-build"
        write_json(record_path, record)
        raise ValueError(f"source changed during build; rebuild from current sources; evidence: {record_path}")
    try:
        resource_hashes(record)
    except (ValueError, OSError) as error:
        record.update(status="input-changed-during-build", error=str(error))
        write_json(record_path, record)
        raise
    try:
        project_inventory(project)
        generated_hashes = record["generated_sha256"]
        if generated_hashes != {name: digest(project / name) for name in GENERATED_FILES}:
            raise ValueError("generated files changed during build")
        built = native_elf(cache / "debug/pal-wayland")
        built_hash = digest(built)
        executable = output / SCHEMA2_PLAYER_PATH
        executable.parent.mkdir(mode=0o700)
        # Copy, never symlink/hardlink to disposable Cargo output. Publish last,
        # after checking both the staged bytes and every captured build input.
        with tempfile.TemporaryDirectory(prefix=".pal-publish-", dir=output.parent) as scratch:
            pending = Path(scratch) / "pal-wayland"
            if pending.parent.stat().st_dev != executable.parent.stat().st_dev:
                raise ValueError("publication staging and bundle must share a filesystem")
            shutil.copyfile(built, pending)
            pending.chmod(0o700)
            if digest(pending) != built_hash or digest(built) != built_hash:
                raise ValueError("native ELF changed during publication")
            if source_snapshot() != record["source_sha256"]:
                raise ValueError("source changed during publication")
            if digest(args.target.resolve()) != record["target_manifest_sha256"]:
                raise ValueError("target manifest changed during publication")
            verify_runtime(ROOT, target)  # Rebind supported identity to the frozen build inputs.
            resource_hashes(record)
            if generated_hashes != {name: digest(project / name) for name in GENERATED_FILES}:
                raise ValueError("generated files changed during publication")
            schema2_structure(output, record, output, publishing=True)
            with pending.open("rb") as stream:
                os.fsync(stream.fileno())
            os.replace(pending, executable)
            directory_fd = os.open(executable.parent, os.O_RDONLY | os.O_DIRECTORY)
            try:
                os.fsync(directory_fd)
            finally:
                os.close(directory_fd)
        record.update(status="ready", executable=str(executable), executable_sha256=built_hash,
                      generated_sha256=generated_hashes)
    except (ValueError, OSError, KeyboardInterrupt) as error:
        record.update(status="failed", error=str(error))
        write_json(record_path, record)
        raise
    write_json(record_path, record)
    print(f"native_elf: {executable}\ngeneration_provenance: {record_path}")
    print(json.dumps({"project": str(output), "native_elf": str(executable),
                      "generation_provenance": str(record_path), "spawned_game": False}))
    return 0


def plain_path(path):
    """Absolute lexical path, rejecting symlinks rather than resolving them away."""
    path = Path(os.path.abspath(path))
    for parent in (path, *path.parents):
        if parent.is_symlink():
            raise ValueError(f"symlink not permitted: {parent}")
    return path


def recorded_path(value, *, absolute=True):
    """Validate frozen lexical paths without consulting the old filesystem."""
    if not isinstance(value, str) or not value or "\x00" in value:
        raise ValueError("invalid recorded path")
    path = Path(value)
    if (path.is_absolute() != absolute or ".." in path.parts
            or str(path) != value or path == Path("/") or path == Path(".")):
        raise ValueError(f"noncanonical recorded path: {value!r}")
    return path


def relocate_path(value, old_root):
    if not isinstance(value, str):
        raise ValueError("invalid recorded path")
    path = recorded_path(value, absolute=Path(value).is_absolute())
    if path.is_absolute():
        if not path.is_relative_to(old_root):
            raise ValueError("recorded resource outside recorded root")
        path = path.relative_to(old_root)
    return plain_path(ROOT / path)


def relocated_target(record):
    old_root = recorded_path(record["root"])
    target = dict(record["target"])
    # Only these schema paths are runtime inputs. Logs, commands, source hashes,
    # tool identities and unknown metadata remain frozen, not recursively rebased.
    for key in ("exe", "assets", "cd_bin"):
        target[key] = str(relocate_path(target[key], old_root))
    return validate_target(target)


def resource_hashes(record):
    target = relocated_target(record)
    old_root = recorded_path(record["root"])
    expected_inputs = {target_path(target[key]) for key in ("exe", "cd_bin")}
    expected_inputs.add(target_path(target["cd_bin"]).with_suffix(".cue"))
    assets = target_path(target["assets"])
    asset_files = {path for p in assets.rglob("*") if (path := plain_path(p)).is_file()}
    for key, expected in (("input_sha256", expected_inputs),
                          ("asset_sha256", asset_files)):
        hashes = record.get(key)
        if not isinstance(hashes, dict):
            raise ValueError(f"missing {key} provenance")
        mapped = {}
        for value, sha256 in hashes.items():
            path = relocate_path(str(recorded_path(value)), old_root)
            if path in mapped or path not in expected:
                raise ValueError(f"inconsistent {key} path inventory")
            mapped[path] = sha256
        if set(mapped) != expected or any(digest(path) != sha for path, sha in mapped.items()):
            raise ValueError(f"resources differ from {key} provenance")
    return target


def native_elf(path):
    path = plain_path(path)
    if not path.is_file() or not stat.S_ISREG(path.stat().st_mode) or not os.access(path, os.X_OK):
        raise ValueError("missing native ELF in ready generation")
    with path.open("rb") as stream:
        if stream.read(4) != b"\x7fELF":
            raise ValueError("native ELF differs from ready generation provenance")
    return path


def exact_tree(directory, files, directories):
    """A manifest never grants authority to add paths or special file types."""
    directory = plain_path(directory)
    if not directory.is_dir():
        raise ValueError(f"missing bundle directory: {directory}")
    actual_files, actual_dirs = set(), set()
    for path in directory.rglob("*"):
        path = plain_path(path)
        name = path.relative_to(directory).as_posix()
        mode = path.stat().st_mode
        if stat.S_ISDIR(mode):
            actual_dirs.add(name)
        elif stat.S_ISREG(mode):
            actual_files.add(name)
        else:
            raise ValueError("special file not permitted in bundle")
    if actual_files != set(files) or actual_dirs != set(directories):
        raise ValueError(f"bundle file inventory differs from approved layout: {directory}")


def project_inventory(project):
    exact_tree(project, GENERATED_FILES, {"src"})


def schema2_structure(bundle, record, old_bundle, *, publishing=False):
    old_root = recorded_path(record.get("root"))
    expected_cache = old_root / "target/native" / bundle.name
    if (record.get("logs") != str(old_bundle / "logs")
            or record.get("build_cache") != str(expected_cache)):
        raise ValueError("schema 2 logs/cache outside approved layout")
    profile = record.get("profile")
    if not isinstance(profile, dict) or profile != build_profile(profile.get("name")):
        raise ValueError("schema 2 requires exact recorded build profile")
    expected_env = {"CARGO_TARGET_DIR": str(expected_cache), "CARGO_INCREMENTAL": "0"}
    expected_env.update({"CARGO_PROFILE_DEV_" + key.upper(): str(value).lower()
                         for key, value in profile.items() if key not in ("name", "cargo_profile")})
    if record.get("build_environment") != expected_env:
        raise ValueError("schema 2 build environment differs from recorded profile/cache")
    logs = plain_path(bundle / "logs")
    if not logs.is_dir():
        raise ValueError("missing schema 2 logs directory")
    # Missing tools need no logs. Only fixed, nonrecursive log names are allowed.
    log_names = {path.name for path in logs.iterdir()}
    if not log_names <= LOG_FILES:
        raise ValueError("schema 2 logs outside approved inventory")
    files = {"generation.json", *('project/' + name for name in GENERATED_FILES),
             *('logs/' + name for name in log_names)}
    if not publishing:
        files.add('bin/pal-wayland')
    directories = {"project", "project/src", "bin", "logs"}
    if (bundle / "assembly.json").exists():
        files |= {"assembly.json", "player-config/SETUP.DAT"}
        directories.add("player-config")
    exact_tree(bundle, files, directories)


def player_path(record, old_bundle):
    if type(record.get("schema")) is not int or record["schema"] not in (1, 2):
        raise ValueError("player requires matching schema 1 or 2, ready generation provenance")
    if record["schema"] == 1:
        return schema1_player_path(record, old_bundle)
    if recorded_path(record.get("executable")) != old_bundle / SCHEMA2_PLAYER_PATH:
        raise ValueError("player executable outside allowed schema 2 layout")
    return SCHEMA2_PLAYER_PATH


def schema1_player_path(record, old_bundle):
    """Select only the current layout or an explicitly allowed frozen legacy one."""
    if type(record.get("schema")) is not int or record["schema"] != 1:
        raise ValueError("player requires matching schema 1, ready generation provenance")
    executable = recorded_path(record.get("executable"))
    if executable == old_bundle / PLAYER_PATH:
        return PLAYER_PATH
    for legacy in LEGACY_SCHEMA1_PLAYER_PATHS:
        if executable == old_bundle / legacy:
            return legacy
    raise ValueError("player executable outside allowed schema 1 layout")


def player_assembly(bundle, generation, generation_hash):
    metadata = plain_path(bundle / "assembly.json")
    if not metadata.exists():
        return None
    record = json.loads(metadata.read_text())
    if not isinstance(record, dict):
        raise ValueError("player assembly must be a JSON object")
    # Assembly has no root field. Its root-pal entry anchors its bundle and ELF;
    # it may have been assembled before OR after the repository relocation.
    entry = recorded_path(record.get("entry"))
    old_root = entry.parent
    old_bundle = old_root / bundle.relative_to(plain_path(ROOT))
    relative_player = player_path(generation, recorded_path(generation["bundle"]))
    config = record.get("player_config")
    if (type(record.get("schema")) is not int or record["schema"] != generation["schema"] or record.get("status") != "assembled"
            or entry.name != "pal"
            or old_root not in (recorded_path(generation["root"]), plain_path(ROOT))
            or record.get("bundle") != str(old_bundle)
            or record.get("executable") != str(old_bundle / relative_player)
            or record.get("generation_sha256") != generation_hash
            or record.get("executable_sha256") != generation["executable_sha256"]
            or record.get("spawned_game") is not False
            or not isinstance(config, dict)
            or config.get("destination") != str(old_bundle / "player-config/SETUP.DAT")
            or config.get("provenance") != "explicit user-supplied original configuration UI output; bytes unmodified"
            or type(config.get("size")) is not int or config["size"] != 20):
        raise ValueError("player assembly differs from ready generation provenance")
    # Source is historical evidence, not an authority to read an arbitrary file.
    recorded_path(config.get("source"))
    seed = plain_path(bundle / "player-config/SETUP.DAT")
    if not seed.is_file() or seed.stat().st_size != 20 or digest(seed) != config.get("sha256"):
        raise ValueError("player seed differs from assembly provenance")
    return record


def ready_player(bundle):
    bundle = plain_path(bundle)
    generated = plain_path(ROOT / "generated")
    if bundle == generated or not bundle.is_relative_to(generated):
        raise ValueError("player project must be a build bundle below repository generated/")
    provenance = plain_path(bundle / "generation.json")
    record = json.loads(provenance.read_text())
    if not isinstance(record, dict):
        raise ValueError("generation provenance must be a JSON object")
    old_root = recorded_path(record.get("root"))
    old_bundle = old_root / bundle.relative_to(plain_path(ROOT))
    if (type(record.get("schema")) is not int or record["schema"] not in (1, 2) or record.get("status") != "ready"
            or record.get("bundle") != str(old_bundle)
            or record.get("project") != str(old_bundle / "project")):
        raise ValueError("player requires matching schema 1 or 2, ready generation provenance")
    relative_player = player_path(record, old_bundle)
    if record["schema"] == 2:
        if bundle.parent != generated:
            raise ValueError("schema 2 bundle must be a direct child of generated/")
        schema2_structure(bundle, record, old_bundle)
        if "generated_sha256" not in record:
            raise ValueError("schema 2 requires generated file inventory")
    executable = native_elf(bundle / relative_player)
    if digest(executable) != record.get("executable_sha256"):
        raise ValueError("native ELF differs from ready generation provenance")
    resource_hashes(record)
    if "generated_sha256" in record:
        hashes = record["generated_sha256"]
        if not isinstance(hashes, dict):
            raise ValueError("generated_sha256 must be a JSON object")
        if set(hashes) != set(GENERATED_FILES):
            raise ValueError("generated file inventory differs from generation provenance")
        for value, sha256 in hashes.items():
            path = plain_path(bundle / "project" / recorded_path(value, absolute=False))
            if digest(path) != sha256:
                raise ValueError("generated file differs from generation provenance")
    generation_hash = digest(provenance)
    player_assembly(bundle, record, generation_hash)
    return bundle, executable, record, generation_hash


def entry_state(entry):
    """Only the repo's task-owned, provenance-verified player link may be replaced."""
    entry = Path(os.path.abspath(entry))
    if entry != ROOT.resolve() / "pal":
        raise ValueError("entry must be repository root pal (no arbitrary file replacement)")
    plain_path(entry.parent)
    if not entry.is_symlink():
        if entry.exists():
            raise ValueError("refusing to replace a regular/user-owned player entry")
        return None
    link = os.readlink(entry)
    destination = Path(os.path.abspath(entry.parent / link))
    generated = ROOT.resolve() / "generated"
    relative = destination.relative_to(generated) if destination.is_relative_to(generated) else None
    if relative is not None and len(relative.parts) == 3 and Path(*relative.parts[1:]) == SCHEMA2_PLAYER_PATH:
        bundle = generated / relative.parts[0]
    elif (relative is not None and len(relative.parts) >= 5
          and Path(*relative.parts[-4:]) in ({PLAYER_PATH} | LEGACY_SCHEMA1_PLAYER_PATHS)):
        bundle = destination.parents[3]
    else:
        raise ValueError("refusing to replace a foreign player symlink")
    _, executable, _, _ = ready_player(bundle)
    if destination != executable:
        raise ValueError("refusing to replace a foreign player symlink")
    return link


def assemble(args):
    """Copy an explicitly supplied UI seed and publish a verified ELF link last."""
    bundle, executable, generation, generation_hash = ready_player(args.project)
    entry = Path(os.path.abspath(args.entry))
    previous = entry_state(entry)  # Fail before copying when the entry is not ours.
    source = plain_path(args.player_config)
    fd = os.open(source, os.O_RDONLY | os.O_NOFOLLOW | os.O_NONBLOCK)
    with os.fdopen(fd, "rb") as stream:
        info = os.fstat(stream.fileno())
        if not stat.S_ISREG(info.st_mode) or info.st_size != 20:
            raise ValueError("explicit UI-authored player config must be a regular 20-byte SETUP.DAT")
        seed_bytes = stream.read(21)
    if len(seed_bytes) != 20:
        raise ValueError("player config changed while reading")
    seed_hash = hashlib.sha256(seed_bytes).hexdigest()
    target = relocated_target(generation)
    for key, relative in (("exe", "assets/unpacked/PAL.raw.exe"),
                          ("assets", "assets/dos-cd"), ("cd_bin", "assets/GC050.BIN")):
        if target_path(target[key]) != ROOT.resolve() / relative:
            raise ValueError("player assembly requires the native player's repository resource layout")

    config_dir = plain_path(bundle / "player-config")
    seed = plain_path(config_dir / "SETUP.DAT")
    metadata = plain_path(bundle / "assembly.json")
    record = {"schema": generation["schema"], "status": "assembled", "bundle": str(bundle),
              "generation_sha256": generation_hash, "executable": str(executable),
              "executable_sha256": generation["executable_sha256"],
              "player_config": {"source": str(source), "sha256": seed_hash, "size": 20,
                                "destination": str(seed),
                                "provenance": "explicit user-supplied original configuration UI output; bytes unmodified"},
              "entry": str(entry), "spawned_game": False}
    if metadata.exists():
        frozen = player_assembly(bundle, generation, generation_hash)
        if frozen is None:
            raise ValueError("player assembly changed concurrently; entry retained")
        old_source = frozen["player_config"]["source"]
        assembly_root = recorded_path(frozen["entry"]).parent
        # An external UI source is permitted only when explicitly supplied again
        # at its exact original path; repository sources can follow the move.
        same_source = (old_source == str(source) or
                       (recorded_path(old_source).is_relative_to(assembly_root)
                        and relocate_path(old_source, assembly_root) == source))
        if not same_source or frozen["player_config"]["sha256"] != seed_hash:
            raise ValueError("existing player assembly/config differs; use a NEW build bundle")
        record = frozen  # Keep and report the original bytes/paths, never rewrite.
    elif seed.exists() or (config_dir.exists() and not config_dir.is_dir()):
        raise ValueError("refusing to overwrite an existing bundle player config")
    # Stage below ignored generated/, beside the bundle, for atomic publication.
    with tempfile.TemporaryDirectory(prefix=".pal-assemble-", dir=bundle.parent) as scratch:
        scratch = Path(scratch)
        if scratch.stat().st_dev != entry.parent.stat().st_dev or scratch.stat().st_dev != bundle.stat().st_dev:
            raise ValueError("assembly staging, entry and bundle must share a filesystem")
        pending = scratch / "pal"
        pending.symlink_to(os.path.relpath(executable, entry.parent))
        if not metadata.exists():
            # Stage complete files before exclusive publication. A caught failure
            # before the metadata commit must leave the candidate retryable.
            staged_seed, staged_metadata = scratch / "SETUP.DAT", scratch / "assembly.json"
            staged_seed.write_bytes(seed_bytes)
            staged_metadata.write_text(json.dumps(record, ensure_ascii=False, indent=2) + "\n")
            staged_seed.chmod(0o600)
            staged_metadata.chmod(0o600)
            created_dir = not config_dir.exists()
            config_dir.mkdir(mode=0o700, exist_ok=True)
            seed_info = None
            try:
                os.link(staged_seed, seed)  # No overwrite, including dangling symlinks.
                seed_info = seed.lstat()
                os.link(staged_metadata, metadata)  # Complete assembly commit.
            except BaseException:
                # Never delete a pre-existing, replaced or subsequently edited seed.
                if seed_info is not None:
                    try:
                        if seed.lstat() == seed_info and digest(seed) == seed_hash:
                            seed.unlink()
                    except FileNotFoundError:
                        pass
                if created_dir:
                    try:
                        config_dir.rmdir()  # Only remove our empty directory.
                    except OSError:
                        pass
                raise
        # Recheck inputs and the old entry immediately before the only replacement.
        if (digest(source) != seed_hash or digest(seed) != seed_hash
                or digest(executable) != generation["executable_sha256"]
                or digest(bundle / "generation.json") != generation_hash):
            raise ValueError("player assembly inputs changed; entry retained")
        ready_player(bundle)  # Recheck resources and assembly before publication.
        if entry_state(entry) != previous:
            raise ValueError("player entry changed concurrently; entry retained")
        os.replace(pending, entry)
    print(json.dumps(record, ensure_ascii=False))
    return 0


def run(args):
    bundle, executable, record, _ = ready_player(args.project)
    target = relocated_target(record)
    command = run_command(target, executable, args.saves, window=args.window,
                          headless=args.headless, audio_output=args.audio_output,
                          sound_blaster=args.sound_blaster, max_steps=args.max_steps,
                          virtual_step_ns=args.virtual_step_ns)
    if args.print_command:
        print(json.dumps(command, ensure_ascii=False))
        return 0
    saves = validate_saves(args.saves, target_path(target["assets"]))
    saves.mkdir(parents=True, exist_ok=True, mode=0o700)
    # Inherit terminal for interactive use; no shell, configuration injection or fixture clock.
    return subprocess.run(command, cwd=ROOT, check=False).returncode


def parser():
    cli = argparse.ArgumentParser(description=__doc__)
    commands = cli.add_subparsers(dest="command", required=True)
    compile_cli = commands.add_parser("build", help="generate and compile; never open window/audio")
    compile_cli.add_argument("--target", type=Path, default=ROOT / "docs/native-target.json")
    compile_cli.add_argument("--output", type=Path, help="new directory below repository generated/")
    compile_cli.add_argument("--profile", choices=("player", "dev"), default="player",
                             help="player: opt-level 3; dev: opt-level 1; both retain checks, publish bundle/bin/pal-wayland")
    compile_cli.add_argument("--profiling", action="store_true",
                             help="compile coarse host spans; activate only with PAL_PROFILE=1 at runtime")
    compile_cli.add_argument("--build-timeout", type=int, default=600,
                             help="bounded timeout per child in seconds (1..1800)")
    assembly = commands.add_parser("assemble", help="install a ready ELF and explicit original-UI seed; no rebuild or runtime")
    assembly.add_argument("--project", type=Path, required=True, help="ready build_bundle printed by build")
    assembly.add_argument("--player-config", type=Path, required=True,
                          help="explicit user-supplied original configuration UI SETUP.DAT (20 bytes); copied unchanged")
    assembly.add_argument("--entry", type=Path, default=ROOT / "pal",
                          help="repository root pal; existing foreign links/regular files are refused")
    launch = commands.add_parser("run", help="run a persistent build bundle; defaults headless/silent")
    launch.add_argument("--project", type=Path, required=True, help="build_bundle printed by build")
    launch.add_argument("--saves", type=Path, required=True, help="explicit separate writable COW directory")
    launch.add_argument("--window", choices=("yes", "no"), default="no")
    launch.add_argument("--headless", action="store_true")
    launch.add_argument("--audio-output", choices=("off", "pipewire"), default="off")
    launch.add_argument("--sound-blaster", choices=("yes", "no"), default="no")
    launch.add_argument("--max-steps", type=int, help="explicit instruction budget; native headless default is 1000000")
    launch.add_argument("--virtual-step-ns", type=int, help="headless replay clock; requires audio off")
    launch.add_argument("--print-command", action="store_true", help="print JSON argv without creating saves or starting devices")
    return cli


def main(argv=None):
    cli = parser()
    args = cli.parse_args(argv)
    try:
        return {"build": build, "assemble": assemble, "run": run}[args.command](args)
    except (ValueError, OSError, KeyError, TypeError) as error:
        print(f"native-game: {error}", file=sys.stderr)
        return 2
    except KeyboardInterrupt:
        return 130


if __name__ == "__main__":
    sys.exit(main())
