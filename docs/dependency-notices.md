# Dependency scope and notices

## Build-time instruction analysis

`iced-x86 = 1.21.0` is pinned in Cargo.lock and enabled only by the `analysis` feature. Its MIT license is compatible with the project tool license; selected features are `std`, `decoder`, `instr_info`, `intel`. Encoder, assembler, and default formatting bundles are disabled. The crate archive has no build.rs. Its exact-version upstream license is at https://github.com/icedland/iced/blob/v1.21.0/LICENSE.txt .

Generated projects depend on this package with `default-features = false`. The decoder must not be linked into the native game runtime; verify the generated project using `cargo tree -e features` and binary inspection, not just the root project's feature list.

The current analysis-only transitive dependency is `lazy_static = 1.5.1`, MIT OR Apache-2.0. Cargo.lock records exact checksums.

## Runtime extended-precision arithmetic

`rustc_apfloat = 0.2.3+llvm-462a31f5a5ab` is pinned by Cargo.lock (`=0.2.3`
in Cargo.toml; Cargo ignores semver build metadata for requirements). It is a
pure-Rust software arithmetic port, NOT the C++ implementation or a CPU decoder.
License: Apache-2.0 WITH LLVM-exception; see [license](../third-party/rustc-apfloat-LICENSE.txt)
and [license details](../third-party/rustc-apfloat-LICENSE-DETAILS.md).
The published build.rs declares `rerun-if-changed` and verifies that the LLVM
revision in `src/lib.rs` matches the package version metadata, failing on a
mismatch. It does not download code, compile C/C++, or execute a native probe.
The upstream license-details copy is retained verbatim; its LLVM Foundation
relicensing URL is no longer available. The official
[LLVM relicensing update](https://blog.llvm.org/posts/2022-08-14-relicensing-update/)
provides historical context.
Runtime transitive dependencies: bitflags 2.13.2, smallvec 1.16.3 (MIT OR
Apache-2.0). Cargo.lock contains exact versions/checksums. The typed finite x87 subset is integrated in `src/fpu.rs`; this is separate
from translating the original guest software-FPU handlers and does not prove
complete Borland/x87 compatibility.

The first supported target is full extended precision (64-bit significand),
explicit rounding modes and finite arithmetic. Other precision controls,
exception/trigonometric behavior must fail until separately implemented and
validated; do not replace x87 computations with host f64.

## Runtime player configuration

Player configuration uses `serde = 1.0.229` with derive and `toml =
1.1.8+spec-1.1.0` with std/serde/parse (both MIT OR Apache-2.0). Cargo requirement
pins `=1.1.8` since semver build metadata does not constrain selection. Runtime
parsing keeps bounded file reads and the parser recursion limit; no filesystem
watcher daemon or notify dependency is added. Sources:
https://github.com/serde-rs/serde and https://github.com/toml-rs/toml .

## Optional native window

`winit = 0.30.12` and `softbuffer = 0.4.8` are pinned in Cargo.lock and enabled
only by `window`, with defaults disabled and Wayland/dlopen features selected
(`rwh_06` also for winit). Winit is Apache-2.0; softbuffer is MIT OR Apache-2.0. Upstream notices:
https://github.com/rust-windowing/winit/tree/v0.30.12 and
https://github.com/rust-windowing/softbuffer/tree/v0.4.8 . Resolved transitive
versions/checksums are in Cargo.lock; lockfile entries for other platforms do
not establish active Linux features. These third-party crates may use platform
FFI; Rust project ownership is not a claim that system libraries are Rust.

`wgpu = 30.0.1` (MIT OR Apache-2.0) and `pollster = 0.4.0` (MIT) are pinned
under `window`. Wgpu defaults are disabled; only std, parking_lot, Vulkan and
WGSL are selected, not GLES/X11 renderer fallback. Upstream:
https://github.com/gfx-rs/wgpu/tree/v30.0.1 and
https://github.com/zesterer/pollster . Wgpu/core/hal/naga build scripts declare
cfg aliases; shaders are WGSL, and game logic remains the Rust AOT module.
Runtime needs the system Vulkan loader/driver. Offscreen GPU validation and
Wayland surface/physical display acceptance are separate checks.
The optional `profiling` feature is a local coarse-duration collector; it adds
no profiler daemon, tracing dependency or per-instruction logging.

## In-process audio and filesystem services

Window mode uses system Wayland/xkbcommon; audio uses `libpipewire-0.3`.
PipeWire, rtrb and rustix are unconditional runtime dependencies, not gated by
`window`; selecting `--audio-output off` does not remove their build requirements.
PipeWire playback is in-process; production does not execute `pw-cat`. The explicit trusted
helper override remains only for diagnostics. `pipewire = 0.10.1` and its
`pipewire-sys` / `libspa` / `libspa-sys` bindings are MIT; they require system
PipeWire/SPA headers, pkg-config and libclang for build-time bindgen. Their build
scripts generate bindings and build wrapper glue, not game logic. Upstream:
https://gitlab.freedesktop.org/pipewire/pipewire-rs .
The fixed-capacity SPSC FIFO uses `rtrb = 0.3.2` (MIT OR Apache-2.0).
DOS capacity queries use `rustix = 1.1.5` / `fs` (MIT OR Apache-2.0 WITH
LLVM-exception); no external `df` process is used. Versions/checksums and
transitive native-binding build dependencies are pinned in Cargo.lock.
PipeWire source/license: https://gitlab.freedesktop.org/pipewire/pipewire . These
are external platform components, not game logic or an x86 CPU fallback.
Transport writes are not evidence of audible device playback. Generated/bundled
third-party dependency distribution requires its own applicable notices.

## Other research references and game rights

Deark (https://github.com/jsummers/deark), SR (https://github.com/M-HT/SR),
masm2c (https://github.com/xor2003/masm2c), asm2c
(https://github.com/frranck/asm2c) are research tools/references, not silently
bundled project dependencies. SDLPAL (https://github.com/sdlpal/sdlpal) may be
used to study formats/mechanisms; it is not the original binary's exact oracle
or a replacement for its algorithms. Copying/porting reference code requires
its license and attribution, not merely a link.

The producer-owned scratch-buffer pattern was also reviewed in the Rust PAL
port's [audio.rs](https://github.com/madeye/Legend-of-Sword-and-Fairy/blob/8e53be81546064c0fcf93995f5347d63affc001f/src/audio.rs)
at revision `8e53be81546064c0fcf93995f5347d63affc001f` (GPL-3.0-only).
The local CD/output buffer reuse is authored against this project's existing
contracts; no engine, DBOPL code, game data or new dependency was imported.
OPL synthesis remains unimplemented.

The same revision's [game_loop.rs](https://github.com/madeye/Legend-of-Sword-and-Fairy/blob/8e53be81546064c0fcf93995f5347d63affc001f/src/game_loop.rs)
provides a palette-lookup presentation reference. The local packed-u32 stack
LUT and removal of redundant non-transaction snapshots are authored adaptations;
original REP/DAC transaction rules remain intact and no upstream engine code
was imported.

`cristicbz/rust-doom` at revision `6aa7681cee4e181a2b13ecc9acfa3fcaa2df4014`
was inspected as a presentation/testing reference:
https://github.com/cristicbz/rust-doom/tree/6aa7681cee4e181a2b13ecc9acfa3fcaa2df4014 .
Its code is Apache-2.0; it is an independent Doom renderer/level viewer, not an
original-DOS execution oracle. No code, WAD data or dependency was imported.
Future copying must satisfy the license/attribution/modification and applicable
NOTICE requirements; the code license does not license external Doom WAD files.
Mechanism candidates and measurement gates are in [development.md](development.md#外部实现借鉴的边界).

The project tool license is [GPL-3.0-or-later](../LICENSE). It does not license
the original game, proprietary input fragments, translated game code, saves,
audio/images or derived ELF. No redistribution permission has been established
for those inputs/derivatives; historical hashes/provenance are retained in the
[resource manifest](resource-manifest.json) and [CD manifest](cd-audio-manifest.json).
The locally inspected resources came from the Internet Archive community archive
https://archive.org/details/pal1_1995 (download: https://archive.org/download/pal1_1995/GC050.BIN);
this is not an authenticated official release or a redistribution grant.
Static unpacking used Deark revision `8f2db0af0b5e322eb19a7d9cf75505b8bf236a5d`
(PKLITE followed by EXEPACK); resulting file identities remain in the resource manifest.

## Development-only reference execution

Unicorn 2.1.4 and Capstone 5.0.7 are isolated Python development dependencies, not Cargo dependencies. Their role is a CPU/reference oracle for differential tests, never a runtime fallback. Unknown host services fail, no GUI/audio is opened, and guest paths cannot be mapped directly to host writes. Reference execution does not count as native game execution.

## pipewire-rs MIT notice

Copyright The pipewire-rs Contributors.

Permission is hereby granted, free of charge, to any person obtaining a
copy of this software and associated documentation files (the "Software"),
to deal in the Software without restriction, including without limitation
the rights to use, copy, modify, merge, publish, distribute, sublicense,
and/or sell copies of the Software, and to permit persons to whom the
Software is furnished to do so, subject to the following conditions:

The above copyright notice and this permission notice (including the next
paragraph) shall be included in all copies or substantial portions of the
Software.

THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT.  IN NO EVENT SHALL
THE AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING
FROM, OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER
DEALINGS IN THE SOFTWARE.

## iced project MIT notice

https://github.com/icedland/iced
Copyright (C) 2018-present iced project and contributors

Permission is hereby granted, free of charge, to any person obtaining
a copy of this software and associated documentation files (the
"Software"), to deal in the Software without restriction, including
without limitation the rights to use, copy, modify, merge, publish,
distribute, sublicense, and/or sell copies of the Software, and to
permit persons to whom the Software is furnished to do so, subject to
the following conditions:

The above copyright notice and this permission notice shall be
included in all copies or substantial portions of the Software.

THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND,
EXPRESS OR IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF
MERCHANTABILITY, FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT.
IN NO EVENT SHALL THE AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY
CLAIM, DAMAGES OR OTHER LIABILITY, WHETHER IN AN ACTION OF CONTRACT,
TORT OR OTHERWISE, ARISING FROM, OUT OF OR IN CONNECTION WITH THE
SOFTWARE OR THE USE OR OTHER DEALINGS IN THE SOFTWARE.
