#!/usr/bin/env python3
"""Opt-in production Wayland renderer lifecycle in gamescope's headless backend.
Usage: python tests/run_gpu_wayland.py /absolute/path/to/lib-test-binary
Never inherits desktop/audio endpoints; all writable state stays in TMPDIR.
"""
import os
import pathlib
import subprocess
import sys
import tempfile

if len(sys.argv) > 1 and sys.argv[1] == "--client":
    env = dict(os.environ)
    for key in ("DISPLAY", "WAYLAND_SOCKET", "PIPEWIRE_REMOTE", "PULSE_SERVER", "DBUS_SESSION_BUS_ADDRESS"):
        env.pop(key, None)
    env["PAL_PRIVATE_WAYLAND_TEST"] = "1"
    # Keep protocol and client outcomes in separate files: compositor logs may
    # interleave a single Rust eprintln into multiple physical lines.
    fd = os.open(os.path.join(env["XDG_RUNTIME_DIR"], "client.log"), os.O_WRONLY | os.O_CREAT | os.O_TRUNC, 0o600)
    os.dup2(fd, 1)
    os.dup2(fd, 2)
    os.close(fd)
    os.execve(sys.argv[2], [sys.argv[2], "--exact", "wayland::tests::isolated_surface_lifecycle", "--ignored", "--nocapture", "--test-threads=1"], env)

binary = pathlib.Path(sys.argv[1]).resolve(strict=True)
script = pathlib.Path(__file__).resolve()
scratch = pathlib.Path(os.environ["TMPDIR"]).resolve(strict=True)
if not scratch.is_dir():
    raise SystemExit("TMPDIR must be a directory")
failures = []
for renderer in ("wgpu", "softbuffer"):
    with tempfile.TemporaryDirectory(prefix="pal-gpu-wayland-", dir=scratch) as temp:
        runtime = pathlib.Path(temp)
        env = {"PATH": "/usr/bin:/bin", "HOME": temp, "XDG_RUNTIME_DIR": temp,
               "PAL_TEST_RENDERER": renderer, "PAL_PRIVATE_WAYLAND_RUNTIME": temp,
               "WAYLAND_DEBUG": "1", "TMPDIR": temp,
               "XDG_CACHE_HOME": temp, "XDG_CONFIG_HOME": temp, "XDG_DATA_HOME": temp}
        command = ["bwrap", "--ro-bind", "/", "/", "--bind", temp, temp,
                   "--tmpfs", "/run", "--tmpfs", "/tmp", "--dev-bind", "/dev", "/dev",
                   "--proc", "/proc", "--unshare-pid", "--unshare-net", "--die-with-parent",
                   "gamescope", "--backend", "headless", "--expose-wayland",
                   "-W", "960", "-H", "600", "-r", "60", "--",
                   sys.executable, str(script), "--client", str(binary)]
        result = subprocess.run(command, env=env, stdout=subprocess.PIPE, stderr=subprocess.STDOUT,
                                timeout=20, text=True)
        log = scratch / f"pal-gpu-wayland-{renderer}.log"
        client_text = (runtime / "client.log").read_text() if (runtime / "client.log").exists() else ""
        combined = result.stdout + client_text
        log.write_text(combined)
        lines = client_text.splitlines()
        attaches = sum("wl_surface#" in s and ".attach(" in s for s in lines)
        commits = sum("wl_surface#" in s and ".commit(" in s for s in lines)
        evidence = [s for s in lines if "lifecycle renderer=" in s or "renderer=" in s]
        client_ok = "test result: ok. 1 passed" in client_text and any("cleanup=yes" in s for s in evidence)
        if renderer == "wgpu":
            resized_dmabuf = any(".create_immed(" in s and ", 641, 401," in s for s in lines)
            client_ok = client_ok and resized_dmabuf
        print(f"{renderer}: compositor_exit={result.returncode} client_ok={client_ok} attaches={attaches} commits={commits} log={log}")
        print("\n".join(evidence))
        if not client_ok or attaches < 12 or commits < 12:
            print(combined[-4000:])
            failures.append(f"{renderer}: client/surface failure")
        if result.returncode != 0:
            failures.append(f"{renderer}: compositor shutdown exit {result.returncode}")
if failures:
    raise SystemExit("; ".join(failures))
