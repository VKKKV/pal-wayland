# 原生 AOT 开发与维护

[README](../README.md) 负责玩家入门；[PLAN](PLAN.md) 固定需求；
[功能账本](functionality.md) 记录当前能力、缺口与验证边界。
本指南只说明维护操作和影响改动的契约。

## 依赖与架构入口

构建需要 Python ≥ 3.11、Rust/Cargo 1.97+、本机 linker、pkg-config、libclang 和 PipeWire/SPA 开发头文件；
即使 audio off 也需要后两者。窗口运行另需 Wayland、Vulkan/GPU、libxkbcommon；声音需 PipeWire 服务。
版本以 [Cargo.toml](../Cargo.toml)/Cargo.lock 为准。首次缓存可用 `cargo fetch --locked`，下面的构建不联网补依赖。

源码分组、AOT 数据流与 feature 边界见 [README 架构节](../README.md#项目架构)；模块职责契约见下节。
本地 `assets/` 是原资源，根目录 `analysis/` 是 Git 忽略的私有调查记录，与源码 `src/analysis/` 不同；`generated/` 保存生成源码、持久 ELF 与构建身份，
构建产物不是可整体删除的缓存；`target/` 保存可重建编译缓存。

`pal-wayland-tool inspect EXE`、`load-check EXE SEGMENT` 只读检查，不执行；加载段必填，接受十进制或 `0x`/`0X`。
`pal-wayland-lift MZ_EXE NEW_OUTPUT ...` 只生成，拒绝覆盖；没有独立成功的 `--help`，完整参数见源码 `USAGE`。

## 架构职责契约

参考 [AnyPS5 的架构](https://github.com/boykopovar/AnyPS5/blob/091c136ba6ef7938283353a6c9ef6ba002ec25ab/docs/dev/ARCHITECTURE.md)
仅借鉴转换工具与平台兼容层的分责，不复制其 PRX/NID 动态链接体系。它保留可直接执行的 x86-64 机器码并重链接、局部修补；
本项目必须转换 DOS 16-bit 分段执行语义，不能以改 ELF header 或替换 imports 代替 AOT。
客体服务链接在同一进程中不等于没有运行库；也不因此引入新 crate、通用服务 trait 或第二种 CPU 执行路径。

- **转换期**：`mz.rs` 加载/重定位；`analysis/lift.rs` 的 recover、静态变体验证、emit 只处理构建期代码。
  PAL 专属入口、跳转表与可变立即数由 `native-target.json` 提供，分析视图仅供恢复代码，不能替换运行时内存。
  CLI 先完成分析，再独占创建生成项目；构建封装负责来源/产物身份，运行直接使用 bundle 内 ELF。
- **客体状态与路由**：`Machine` 唯一持有寄存器、内存、时序与设备状态。
  `software_interrupt` 先检查 IVT，已安装的 guest handler 优先；原指令进入该 handler 后仍由静态代码执行。
  默认 INT 21h 才进入私有 [DOS ABI 实现](../src/runtime/dos.rs)，它仍是 `impl Machine`，不复制状态或扩大公开 API。
- **服务与机制**：DOS 模块封送寄存器/guest 内存，保持 AX/CF、文件位置与失败时的副作用顺序；
  [DosFiles](../src/devices/dos_files.rs) 管理 guest namespace、句柄、只读资源/COW 存档与实际文件操作。
  BIOS/MSCDEX、端口和 IRQ 路由仍在 `runtime.rs`，设备模型仍在 `devices/`；AH=36 的 in-process 容量查询暂留 DOS 模块，
  不为单个 syscall 另造适配接口。未来服务按已有职责补入，不把 guest 路径直接作为 host 路径。
- **宿主消费**：窗口/音频消费已发布的索引帧、palette/mask 和 PCM，输入回送 guest；重绘不推进 guest。
  `host/gpu.rs` 的固定 WGSL 呈现不是 guest shader 重编译，也不需要 AnyPS5 的 PM4/RDNA pipeline 或 shader cache。
- **错误契约**：合法 DOS 错误按 guest 约定返回；未知目标、服务、代码变化或不支持设备状态返回带上下文的 `Fault`，不伪造成功。
  原有 `Machine::fault` 保留 CS:IP、step 与寄存器；新增诊断写明转换阶段或服务号/端口/访问宽度，
  host 故障不得静默切换 CPU 执行机制。生成成功、启动成功、代表性玩法通过分别验收，不以 export 存在代替功能正确。

仅调整 DOS 服务边界时，复用现有代表性测试，无需新增 harness 或穷尽矩阵：

```sh
cargo test --offline --locked --no-default-features --lib runtime:: -- --test-threads=1
cargo test --offline --locked --no-default-features --test dos_capacity -- --test-threads=1
cargo test --offline --locked --test lift_pipeline generate_build_execute_and_reject_overwrite_unknown_and_modified_code -- --exact --test-threads=1
```

最后一项实际生成/编译并执行 authored MZ，验证 INT 21h、guest handler 优先和 decoder 不进入生成 runtime；
这些测试不启动窗口或音频，也不修改原资源、用户配置、存档或现有玩家 ELF。

## 原资源准备

仅支持 [resource manifest](resource-manifest.json) 与 [CD manifest](cd-audio-manifest.json) 的固定输入，不根据文件名推定版本兼容。原零售修订/语言未充分确认，社区来源记录不是授权来源；请自行合法取得资源。准备过程不执行 DOS 游戏，也不下载游戏数据。

构建/运行必需：完整 `assets/GC050.BIN`、匹配的 `assets/GC050.cue`、33 个 `assets/dos-cd/` 数据文件及 `assets/unpacked/PAL.raw.exe`。ISO、PALS 与 PKLITE 中间文件仅用于完整准备复核，不是运行依赖。ISO 不含音乐，不能替代完整 BIN/CUE。

以下在仓库根、独立 shell 中依次执行，需要 Python ≥3.11、7-Zip（`7z`）、tar、make 和 C 编译器；首次获取解包工具还需 curl。每次使用新私有临时目录，原 BIN/CUE 只读，已有提取目录不覆盖。下文全部中间产物在 `W`；复核通过后才安装到 ignored `assets/`。

先自行放置完整 BIN/CUE，检查完整 BIN 身份并准备固定 Deark 源码。已有源码归档可提前设置 `DEARK_ARCHIVE=/absolute/path/to/deark-archive.tar.gz`；否则仅此步联网获取工具，之后离线：

```sh
set -eu
REPO="$PWD"
W=$(mktemp -d)
REV=8f2db0af0b5e322eb19a7d9cf75505b8bf236a5d
python3 - <<'PY'
import hashlib, json
from pathlib import Path
m = json.loads(Path('docs/cd-audio-manifest.json').read_text())
p = Path('assets/GC050.BIN')
with p.open('rb') as f:
    digest = hashlib.file_digest(f, 'sha256').hexdigest()
if p.stat().st_size != m['bytes'] or digest != m['sha256']:
    raise SystemExit('unsupported complete BIN; do not continue')
if not p.with_suffix('.cue').is_file():
    raise SystemExit('missing matching CUE')
PY
if [ -n "${DEARK_ARCHIVE:-}" ]; then
    cp "$DEARK_ARCHIVE" "$W/deark-$REV.tar.gz"
else
    curl --fail --location "https://codeload.github.com/jsummers/deark/tar.gz/$REV" \
        --output "$W/deark-$REV.tar.gz"
fi
printf '%s  %s\n' 87c62224388cb952d074dcfa0d5a7df6dbf8d1709de29db98ad4540ef6065613 \
    "$W/deark-$REV.tar.gz" | sha256sum --check --strict
tar -xzf "$W/deark-$REV.tar.gz" -C "$W"
make -C "$W/deark-$REV" -j2
D="$W/deark-$REV/deark"
"$D" -version
```

该 revision 的 Deark 为 1.7.3-1。提取第一数据轨：track 2 的 INDEX 01 `03:04:37` 对应 13,837 个 2352-byte raw 扇区；每扇区取 offset 16 的 2048-byte payload。末尾 150 个扇区是全零 padding，须保留，不能因没有 MODE1 header 而删除。raw/ISO 的 SHA-256 都先与 manifest 核对：

```sh
python3 - "$W/GC050-track01.iso" <<'PY'
import hashlib, json, sys
from pathlib import Path
m = json.loads(Path('docs/resource-manifest.json').read_text())
with Path('assets/GC050.BIN').open('rb') as f:
    raw = f.read(32544624)
if hashlib.sha256(raw).hexdigest() != m['raw_track_sha256']:
    raise SystemExit('unsupported raw data track')
iso = b''.join(raw[i + 16:i + 2064] for i in range(0, len(raw), 2352))
if hashlib.sha256(iso).hexdigest() != m['iso_sha256']:
    raise SystemExit('ISO identity mismatch')
with Path(sys.argv[1]).open('xb') as f:
    f.write(iso)
PY
7z x -tiso "$W/GC050-track01.iso" -o"$W/dos-cd"
mkdir "$W/unpacked"
for N in PAL PALS; do
    "$D" -m pklite -d -t "$W/unpacked/$N.unpacked.exe" "$W/dos-cd/$N.EXE"
    "$D" -m exepack -d -t "$W/unpacked/$N.raw.exe" "$W/unpacked/$N.unpacked.exe"
done
```

`.unpacked.exe` 仅去除 PKLITE，仍含 EXEPACK；`.raw.exe` 才是两层解包后的最终 MZ。输出与 `unpacked_files` 一一对应，不仅检查 Deark 的退出码：

```sh
python3 - "$W" <<'PY'
import hashlib, json, sys
from pathlib import Path
m = json.loads(Path('docs/resource-manifest.json').read_text())
w = Path(sys.argv[1])
for folder, key in [('dos-cd', 'files'), ('unpacked', 'unpacked_files')]:
    if {p.name for p in (w / folder).iterdir()} != {i['name'] for i in m[key]}:
        raise SystemExit(f'unexpected {folder} inventory')
    for item in m[key]:
        p = w / folder / item['name']
        with p.open('rb') as f:
            digest = hashlib.file_digest(f, 'sha256').hexdigest()
        if p.stat().st_size != item['size'] or digest != item['sha256']:
            raise SystemExit(f'identity mismatch: {p.name}')
print('PASS: 33 data files and four unpack stages')
PY
# 新安装；先检查所有目标，包含悬空 symlink，不部分创建目录。
for P in "$REPO/assets/dos-cd" "$REPO/assets/unpacked" "$REPO/assets/GC050-track01.iso"; do
    if [ -e "$P" ] || [ -L "$P" ]; then
        printf 'refusing existing resource destination: %s\n' "$P" >&2
        exit 1
    fi
done
mkdir "$REPO/assets/dos-cd" "$REPO/assets/unpacked"
cp "$W/dos-cd/"* "$REPO/assets/dos-cd/"
cp "$W/unpacked/"* "$REPO/assets/unpacked/"
cp "$W/GC050-track01.iso" "$REPO/assets/GC050-track01.iso"
python3 tools/verify_assets.py --runtime
python3 tools/verify_assets.py
```

运行资源检查 `--runtime` 不要求 ISO/中间文件，并核对完整 BIN 与 CUE 的轨号、类型和 INDEX；默认检查另复核全部解包阶段与 ISO。已存在资源目录时先执行校验，失败则自行选择新的准备位置，不用覆盖命令修补。构建封装也会在调用 Cargo 前执行同一运行资源检查。

## 构建与运行

在仓库根运行。省略 `--output` 在 `generated/` 下自动创建唯一目录；指定时，该目录及对应原生编译缓存必须尚不存在：

```sh
python3 tools/native_game.py build --profile player --build-timeout 1800
```

[构建封装](../tools/native_game.py) 在创建 bundle/调用 Cargo 前，按资源及 CD manifest 核对 raw EXE、数据文件、完整 BIN 与 CUE 轨道布局；不接受仅路径存在的未知版本。它只生成/编译，不启动游戏，offline/locked 构建 lift，
读取 [native-target.json](native-target.json) 并启用 `--fpu-macros borland`，生成项目 lock，再编译 `window` ELF。
默认每个子进程限时 600s，`--build-timeout` 接受 `1..1800`；失败保留 bundle/日志，不覆盖旧产物，正式重建另选新目录。
成功后将输出的 `native_elf` 赋给 `PAL_ELF`、`build_bundle` 赋给 `PAL_BUNDLE`；直接运行 `"$PAL_ELF"`，无需根目录链接。文档不固定某次构建路径。

- `player` 为 opt-level=3，`dev` 为 opt-level=1；两者使用 Cargo dev 布局，debug=0、incremental=false、
  codegen-units=16、LTO=false，**保留 overflow checks、debug assertions 和代码 guards**，不是 release profile。
- 清除继承的 `CARGO_BUILD_TARGET`，为每次构建分配独立缓存并固定 profile 环境；缓存位置以构建记录为准，
  成功后将 ELF 复制并原子发布到 bundle 的 `bin/pal-wayland`，不依赖缓存内的 ELF。
  发布 staging 使用 `generated/` 下独占的私有临时目录，与 bundle 同文件系统，不依赖 TMPDIR 或外部工具目录；源码、资源、清单及生成文件复核失败不发布 `ready`。
  冲突的 `RUSTFLAGS`/encoded/target rustc flags 在创建 bundle 前拒绝，不静默丢弃或关闭检查。
- 可选 `--profiling` 编入宿主分段计时，运行时还须 `PAL_PROFILE=1`。无 feature 移除埋点，未启用环境变量不采样计时。
  [host_profile.rs](../src/host/host_profile.rs) 使用固定 collector/桶，不保存样本数组；p50/p95/p99 是桶上界，
  upload/submit/present 是 CPU 调用耗时，不是 GPU 完成或物理显示延迟。保持 `PAL_*` 诊断协议兼容。

### 路径与精确 token

- CLI 相对 `--target`/`--output`/`--project`/`--saves` 相对调用者 cwd；清单内资源路径相对仓库根。
- 封装地址必须四位十六进制 `CS:IP`，如 `3524:002a`；table 为 `CS:IP:十进制数量`，如 `3524:0010:13`，
  数量 `1..32768` 且完整 word 表不越段末。**不补零、裁空白或修正 token**；直接 lift 的解析较宽松。
- 规范 PAL 地址基于加载段 `1010h`。`1000h` 下的 `33b4:042c` 不是 runtime 的 `33c4:042c`，也不是 image-relative 地址。
- 清单 `ram_top` 必须四位十六进制，Python 与 Machine/原生 CLI 均仅支持 `9f00..a000`。
- 运行观察不自动修改清单；新目标须证明后进入下一次构建，不能在 runtime 动态解码。

成功打印 `build_bundle`、`native_elf`、`generation_provenance`。**`--project` 接收 bundle，不是其 `project/` 子目录**。

| bundle 内路径                                                      | 用途                                                                                                                         |
| ------------------------------------------------------------------ | ---------------------------------------------------------------------------------------------------------------------------- |
| `generation.json`                                                  | schema 2；清单/资源库存与 SHA-256、实际源码（含 dirty）身份、命令/退出码/超时、profile、cache/logs 路径、生成文件和 ELF 身份 |
| `logs/{tool-build,generate,lock,native-build}.{stdout,stderr}.log` | 构建阶段完整日志；工具/库身份查询日志也在 `logs/`                                                                            |
| `project/`                                                         | Cargo 项目/lock、生成源码、`source-map.txt`、`SCOPE.txt`，不含 Cargo target                                                  |
| `bin/pal-wayland`                                                  | 独立的持久 ELF；不是缓存的 symlink/hardlink                                                                                  |

Cargo 缓存不在 bundle 内；已完成 bundle 的运行不读取该缓存。
清理只针对已核验的精确路径：保留当前完整 bundle，删除旧 bundle 和独立原生编译缓存；不要改写冻结 provenance 或将输出追加到 bundle。

源码/模板或资源在构建前后变化时拒绝 `ready`；修改源码不会自动更新 ignored ELF，Git revision 不能代替实际 ELF 身份。
可选 `build_identity` 记录 rustc/Cargo、host/release 和系统库 pkg-config 身份，不承诺字节级可复现。
每项查询最长 10s，每流日志最多 64 KiB、版本字段 16 KiB、错误摘要 4 KiB；超限明确失败并标记前缀，
不能把截断输出称完整日志。超时/中断清理独占进程组；可选库元数据缺失不等于运行库缺失或新增构建门槛。

`run`/`assemble` 要求 schema 1 或 2、`ready`，复核 ELF、EXE/BIN/cue、**完整资产库存**及声明的生成文件哈希。
`generated_sha256` 存在时必须包含固定六文件：Cargo.toml、Cargo.lock、src/main.rs、src/translated.rs、source-map.txt、SCOPE.txt；
旧 schema 1 缺该字段仍兼容，空/部分清单不能绕过检查。schema 2 必须声明完整六文件库存，
并核验精确 bundle 布局、profile 和记录的 cache/logs 路径；额外文件、symlink 或特殊文件拒绝。
已装配 bundle 另核验 generation/ELF/20-byte seed 身份；assembly schema 与 generation 一致。
诊断输出放 TMPDIR，不追加到 schema 2 bundle 内。
历史源码不必等于当前源码，资源来源身份仍由 [资源 manifest](resource-manifest.json)/[CD manifest](cd-audio-manifest.json) 说明。

### 配置种子与整体迁移

已装配 bundle 的 `player-config/SETUP.DAT` 是原 INSTALL UI 生成的设备配置种子，与宿主 TOML 无关。
运行时只播种缺失的用户设置，不覆盖已有文件；新 build 本身不生成该 seed，不能靠手写 SETUP 字节代替原 UI。
已有 `assembly.json` 与验证记录仅保留构建历史，不是当前的启动步骤；不要改写冻结身份。

整仓迁移后，仅按 schema 认可的 root/bundle/project/ELF 结构和原相对资源布局定位当前文件；
不读取历史根、不递归重写 logs/commands/source hashes 或未知字段。逃逸、symlink、库存/哈希漂移拒绝。

## 配置与启动

下述契约以当前源码为准；验收边界见[功能账本](functionality.md)。

新 package/crate 为 `pal-wayland`/`pal_wayland`，工具为 `pal-wayland-tool`/`pal-wayland-lift`，
生成 ELF、Wayland app_id 和 PipeWire app 为 `pal-wayland`。

| 调用                               | 默认行为                                                                                                                         |
| ---------------------------------- | -------------------------------------------------------------------------------------------------------------------------------- |
| 无 positional EXE（含 flags-only） | 玩家：Wayland、wgpu、PipeWire、SB、OPL absent、IRQ enabled、RAM top a000、legacy13；窗口默认无限运行                             |
| 首参数为原 EXE                     | 诊断：headless、audio off、SB off、IRQ disabled、RAM top 9f00、标准 CD header；无自动资源/saves/seed/用户 TOML，默认预算 1000000 |

玩家沿 canonical ELF 祖先寻找 `assets/unpacked/PAL.raw.exe`、`assets/dos-cd`、`assets/GC050.BIN`＋`GC050.cue`，不猜 cwd。
移动 ELF 要保留布局，或使用 `--resource-root`/`--exe`（玩家模式专用）、`--assets`、`--cd-bin`；`--cd-iso` 是 data-only，无 CD-DA。
`--help`/`-h` 任意位置优先、无文件访问；`--print-config` 校验资源/MZ/设置但不写入或连接设备。
`--headless` 等于 `--window no`，与 `--window yes` 冲突，玩家未显式选择音频时默认 off；配置不突破该静音默认。

### TOML 配置与热加载

默认 `$XDG_CONFIG_HOME/pal-wayland/config.toml`，XDG 未设或为空时用 `$HOME/.config/pal-wayland/config.toml`。
默认文件缺失用内置值；显式 `--config PATH` 缺失失败。`--no-config` 禁用读取/监测；诊断模式可显式选配置。
`--controls` 标明 physical/QWERTY 位置输入与 DOS 目标，为有效映射及禁用项标记 `source=default/user`；不加载资源/设备，只可配 `--config` 或 `--no-config`。
`--init-config` 通过同目录私有 staging 写入并 sync，以 hard-link no-replace 发布 0600 [完整示例](../examples/config.toml)；失败不留最终半文件，可重试，无 hard-link 支持明确失败。已有目标拒绝覆盖，可用 `--config` 选择位置。

[配置解析器](../src/host/player_config.rs) 要求 `version = 1`、普通文件且不超过 64 KiB；未知字段/键名、错误类型、越界值、文件 symlink/FIFO/device 均拒绝。
CLI > TOML > 内置值。TOML 相对路径相对配置文件目录，不展开 shell/变量/`~`；CLI 路径相对 cwd。
键位只使用 `[keymap]` 的宿主键 → DOS 键值对，不支持 preset 或旧 `[input]`；示例显式列出 WASD 与方向键，初始化按示例生成。
左侧按 QWERTY 物理位置命名，右侧为 DOS 键名；软件 Colemak/Dvorak 不改变左侧含义，固件重映射可能改变设备上报键码。
无配置、空 `[keymap]` 或 `--no-config` 时默认 WASD → 上/左/下/右，方向键仍可用；示例的有效键位与内置默认一致。
未列出的键仍保留默认值，显式设置覆盖默认，`"None"` 禁用单键；如需恢复字母可写 `KeyW = "KeyW"`（A/S/D 同理）。
WARS 布局可保留 W/A，将下/右改为 `KeyR = "ArrowDown"`、`KeyS = "ArrowRight"`，再设 `KeyD = "KeyD"` 恢复字母（或 `"None"` 禁用）；修改原有项，不追加重复键。
`--controls` 中内置映射标记 `default`，显式覆盖/禁用标记 `user`，即使填写同一目标也为 `user`。
从旧配置升级时，把原 preset 的实际映射展开成显式键值对，删除 preset 和旧 `[input]`，不要追加重复 `[keymap]`；需使用新构建。
宿主 TOML 管窗口/键位/音量等；存档目录的 `SETUP.DAT` 是原 DOS 游戏设备设置，两者格式与消费者不同。
`examples/config.toml` 通过 `include_str!` 编译进 ELF，其 `[keymap]` 经同一解析器校验后缓存为内置默认键位；不另维护 Rust 默认键位表。
其他字段仍由类型默认值与启动模式决定，不将示例的显式 PipeWire 设置注入 headless/诊断模式。该文件已纳入构建源码哈希。
运行时不读取仓库示例，也不自动创建配置；`--init-config` 导出同一份完整示例，或显式 `--config examples/config.toml` 选择磁盘文件。
初始化的完整配置是用户快照，已写出的键位标记 `user`，不会随未来内置默认变化；只需继承默认时无需初始化，已有文件可仅保留所需覆盖项。

| 字段                             | 值/默认                           | 生效                                       |
| -------------------------------- | --------------------------------- | ------------------------------------------ |
| `video.renderer`                 | wgpu（默认）/ softbuffer          | 重启                                       |
| `audio.output`                   | pipewire / off；省略时按启动模式  | 重启                                       |
| `audio.volume` / `muted`         | 0..100 / bool，默认 100 / false   | 热加载，sink-only，不改捕获或源消费        |
| `keymap.<HostKey>`               | 精确 DOS 键名 / "None"            | 热加载，覆盖内置映射；支持键见 `--controls` |
| `paths.resource_root` / `saves`  | 可选非空路径                      | 重启                                       |
| `reload.enabled` / `interval_ms` | 默认 true / 500，间隔 100..5000ms | 关闭后重新启用须重启                       |

轮询检测同尺寸修改/原子替换；有效 live 项一次提交，其他项提示 restart-required，CLI 固定项不误报。
删除、读取失败或无效 TOML 保留 last-valid，修好后恢复；选定路径不在运行中切换。
换映射释放旧 guest holds，物理键须松开再按；只改音量不释放键，失焦/暂停/退出统一释放。
轮询间隔不是 GPU/宿主阻塞下的硬期限，已排队 PCM 保留提交时增益。

### 资源、namespace 与存档保护

默认 saves 为 `$XDG_DATA_HOME/pal-wayland/saves`，XDG 未设或为空时用 `$HOME/.local/share/pal-wayland/saves`；所用基址必须绝对。
默认配置/存档仅使用 `pal-wayland` namespace；不读取旧 `pal-dos-native` 目录，不自动迁移用户数据。
缺失配置使用内置值，缺失存档目录在后端就绪后创建；无效路径、权限错误或 symlink 明确失败。显式 `--config`/`--saves` 仍优先。
默认 namespace 查找和热重载拒绝 symlink 祖先；显式配置允许正常 dotfiles 父目录链接，但配置文件本身仍不能是链接。

[DosFiles](../src/devices/dos_files.rs) 读取 saves 同名文件优先，再读只读 assets；写入只走独立 COW saves。
Guest C:/D: 是受限平面 DOS 文件名空间，不是 host 路径。默认盘为 C:；AH=36 只查询默认/C: 的 saves 容量，AH=47 返回默认/C:/D: 根目录，其他盘返回 DOS 错误。AH=3D 只支持访问模式 0..2，不实现共享/继承控制。
玩家 saves 与 assets、EXE、CD/cue、seed 必须分离，不能互为父子，拒绝祖先/树内 symlink。
使用私有 saves；不以恶意并发目录替换为可支持场景。

玩家在窗口/所选音频后端成功后才创建/播种/挂载 saves；启动失败不新增设置。
只在缺失 `SETUP.DAT` 时用 bundle seed：同目录 staging 完整写入并 sync，再 hard-link no-replace 发布；失败清理 staging，可重试，无最终半文件。
保留已有大小写变体，歧义/非普通文件拒绝；无 hard-link 支持明确失败，不退回覆盖 rename。
普通保存使用文件 sync＋rename，不承诺断电后目录项持久性。build 和 Python run 不播种，不合并旧 scratch saves。

### 有界诊断

先只读核验候选身份和 argv；移除 `--print-command` 才实际运行并创建显式 COW 目录，无窗口/声音：

```sh
: "${TMPDIR:?set TMPDIR to a private scratch directory}"
: "${PAL_BUNDLE:?set PAL_BUNDLE to the build_bundle printed by build}"
python3 tools/native_game.py run --project "$PAL_BUNDLE" \
  --saves "$TMPDIR/pal-wayland-diagnostic-saves" --headless --max-steps 1000 --print-command
```

封装用 argv、无 shell，显式传清单资源、RAM top、IRQ 和 legacy13，不固定 DOS 时钟。
`--sound-blaster yes` 加 `--opl absent`，实时输出另选 `--audio-output pipewire`；`--window yes` 另需 Wayland。
原生完整 help/限制以 [Startup::HELP](../src/host/native_startup.rs) 为准，常用诊断值：

- `--stop CS:IP` 在地址执行前停止，仅 headless；`--set REGISTER=HEX` 和 `--key-event STEP:SCAN:ASCII:down|up` 可重复，scan/ascii 为十六进制。
- `--trace NEWFILE` 为 pre-step JSONL，`--trace-after N` 按 steps；`--memory`/`--state`/`--frame-ppm` 同为 headless、create-new，窗口也拒绝 scheduled keys。
- `--memory-input FILE` 必须完整 1 MiB，只是 fixture；不含 XMS、IRQ/队列、文件游标或播放状态，不是可恢复 host save。
- `--audio-wav NEWFILE --audio-max-frames N` 为 headless 有界捕获，与实时音频互斥；`--virtual-step-ns N` 为正值，不配实时音频。
- 确定性配对必须分别固定虚拟硬件时间和 DOS 民用时间；`--virtual-step-ns` **不固定 INT21/AH=2C**。
  `--clock-centiseconds` 只接受 u8（0..255），runtime 字段是午夜起累计百分秒；`0` 可固定前缀，旧记录的 `4320000` 当前会拒绝。
- 预算通常以退出码 1、`static execution budget exceeded` 结束，不是正常退出；REP/静态调用边界可能 overshoot。
  REP 每次最多 64 元素，steps 计普通指令或 REP 元素，不是统一 architectural instruction 数。

## 改动时保持的边界

- **加载与内存**：[MZ loader](../src/mz.rs) 验证头/image/完整 relocation、entry/stack，拒绝 overlay 和识别出的 secondary header。
  loaded buffer 从加载段基址开始，不是物理 RAM 0；[Machine](../src/runtime.rs) 才建立 PSP/environment、DS=ES=PSP、FLAGS=0202h 和 A20-disabled RAM。
  段末 word/span 越界失败；CPU、设备、RAM-only stack/code guard 各自预检，不能把 guest 地址变成 host pointer。
- **静态图**：[lift](../src/analysis/lift.rs) 保留 CS:IP/重叠入口/完整 byte guard，只接受确认 roots/tables。
  `--mutable-immediate` 只豁免支持的 MOV operand slot；Borland FPU 补丁是有限例外，其他变化失败。
  `--static-view` 必须完整 1 MiB，后续 `--view-seed`/非空、不重叠、end-exclusive `--view-range` 绑定最近 view；
  view 仅构建分析，不注入 live RAM，guest 自己 copy/patch。未知第三变体拒绝；`--entry`/普通 seeds/tables 仍属于原 MZ。
- **指令/IRQ**：[生成语义](../src/analysis/lift32.rs) 区分 operand/address/stack width；word double-shift count17..31 明确拒绝。
  [runtime](../src/runtime.rs) 在静态指令边界处理 IRQ/shadow/frame；安装的 guest INT vector 进入静态 handler，不回退 host shim。
  未支持 DOS/BIOS/端口服务、无效 frame/返回栈拒绝，不为路线推进写剧情、物品或碰撞状态。
- **设备总线**：[planar VGA](../src/devices/planar_video.rs) 以 planes/latches 为权威，mixed RAM/planar span 和不支持模式拒绝，不能读取 stale RAM shadow。
  [XMS](../src/devices/xms.rs) lock 返回 DX:BX；[DMA/SB](../src/devices/sound_blaster.rs) 使用实际锁定 backing 和固定 page 内 64 KiB 回绕，
  allocation gap、解锁、越块、VGA aperture 在消费前拒绝。audio off/speaker muted 仍推进 DMA/completion；FM/OPL/MIDI 不合成。
- **显示**：[video](../src/devices/video.rs)/[GPU](../src/host/gpu.rs) 保留可证明全屏 REP/DAC 事务快照，普通局部写仍可见；索引、palette、mask 同步冻结。
  raw frame/PPM 保留客体诊断视图；mode3 窗口是黑色占位，不冒充文本显示；mode12 headless 可输出，窗口尺寸仍拒绝。
  GPU/surface 初始化失败不静默降级，softbuffer 须显式选。guest slice 有界不保证 swapchain/compositor 的整体延迟。
- **CD 协议**：[cdrom](../src/devices/cdrom.rs) 以真实 CUE/consumed PCM 推进位置，预取不伪造播放。
  `legacy13` 只放宽已观察的固定前缀，仍预检实际 command/nested spans；非零 short IOCTL count 返回 `8105`，raw subchannel 不支持。
  PLAY84 的 sector count 是 DWORD；QInfo 的 BCD track 与 binary M/S/F、absolute bias 不混用。

### 有界欠账恢复

[实时输出](../src/host/audio_output.rs) 只有一个源消费者，固定 s16ne/44100Hz/stereo。
[进程内 PipeWire](../src/host/pipewire_output.rs) 等格式确认和 Streaming 最多 2s，不启动 `pw-cat`、不自动重连/换后端。
`--audio-helper EXACTPATH` 仅是显式受信任诊断 executable，无 shell/隐式 argv；裸名称按 PATH 查找，需要固定 executable 身份时请显式传绝对路径。连接成功不等于可闻播放。
`sink_written_frames`/helper-written 和欠供补零分别记账，`device_played=unknown` 不能解释为已听到声音。

实时每批最多 **2205 帧**，队列＋在途最多 **4410 帧**；保留 debt，以 **1ms** retry 追平。
Pending 时暂停 guest 指令和 PLAY/PAUSE/RESUME/STOP 转换，不丢帧、不倒拨时钟、不提高上限。
无进展 **250ms** 或恢复超过 **2s** 失败；时间倒退、缺源、FIFO 被其他消费者抽走、helper/worker 失败也明确停止。
[WAV capture](../src/host/audio_capture.rs) 有独立限额/catch-up 契约，正常有界停止仍 finalize，读写失败不伪造 silence success。

### 外部实现借鉴的边界

借鉴机制，不把 SDLPAL、Rust PAL 或 rust-doom 当原二进制 oracle，也不移植它们的游戏算法。
索引 palette、执行/绘制分离和 producer scratch 已有本地实现，不再维护接入计划。
来源版本、URL、版权和完整 notices 见 [第三方声明](dependency-notices.md#other-research-references-and-game-rights)；
改动按相同输入/时钟、组件像素/源样本与实际 caller 检查，不以微测宣称 FPS/显示延迟改善。

## 测试与独立 oracle

默认使用原创 fixture、headless/静音和私有 TMPDIR；测试前创建一个可写私有目录并 export TMPDIR（例如 `export TMPDIR="$(mktemp -d)"`），不要求固定 home 或 Agent 环境。Linux 配置写入失败回归用 Python 子进程设置文件大小限制，不改变测试父进程。私有 PipeWire 测试另需 `pipewire` daemon，开发探索需 `bwrap`；可选 GPU lifecycle runner 需 `gamescope` 与 `bwrap`，始终使用 headless compositor，不连接桌面。嵌套 Cargo 共用 [tests/common](../tests/common/mod.rs)：
默认 `$TMPDIR/pal-wayland-test-target`，可选 `PAL_NATIVE_TEST_TARGET_DIR`（相对仓库根），不能复用玩家或外层 target。
按 feature/profile/environment 分区并回收各 fixture ELF 身份，不另建 harness/cache。

测试保持 suckless：少量代表性用例优先走真实 caller，不重复编译生产模块、不锁定生成文本写法，不为新功能展开穷举矩阵或新增 harness。
按改动选测试，不为改稿重跑全套；语义改动须执行生成程序/实际 caller，不能只看生成字符串。

```sh
: "${TMPDIR:?set TMPDIR to a private scratch directory}"
cargo test --offline --locked --test native_startup --test player_config
PYTHONDONTWRITEBYTECODE=1 python3 -m unittest discover -s tests -p test_native_game.py
```

需要完整回归时：

```sh
: "${TMPDIR:?set TMPDIR to a private scratch directory}"
cargo test --offline --locked
PYTHONDONTWRITEBYTECODE=1 python3 -m unittest discover -s tests -p 'test_*.py'
cargo fmt --all --check
cargo clippy --offline --locked --all-targets --features window -- -D warnings
RUSTDOCFLAGS="-D warnings" cargo doc --offline --locked --no-deps
```

真实资源另行 opt-in；缺失/身份错误应失败，不能算跳过后的全部通过：

```sh
python3 tools/verify_assets.py
python3 -O tools/verify_assets.py
PAL_MZ_TEST_INPUT="$PWD/assets/unpacked/PAL.raw.exe" \
  cargo test --offline --locked --lib mz::tests::local_pal_raw_static_acceptance -- --ignored --exact
cargo test --offline --locked --test pal_assets --test cdrom_audio --test audio_pipeline -- --ignored
```

上述命令只选择 MZ/重定位与 CD 原始样本资源检查，不运行其他 ignored 测试或内部子进程入口。
[pal_assets](../tests/pal_assets.rs) 直接读取仓库路径；CD 测试默认读取 `assets/GC050.BIN` 和 `.cue`，可用 `PAL_CD_ASSET_DIR` 指定目录。
窗口/GPU/真实音频需相应授权；surface、PCM 传输、源样本、可闻声音和游戏正常退出不是同一后置条件。

### 独立 CPU oracle

[oracle_trace.py](../tools/oracle_trace.py) 使用独立 MZ loader 和 Unicorn 2.1.4/Capstone 5.0.7（隔离开发依赖，不进入 runtime）。
只实现声明的整数 allowlist 和 INT21 25h/30h/35h/4400h/4Ah/4Ch；未知服务、端口 I/O/其他指令失败，无资源挂载、窗口或声音。
DOS shim 与选定 Machine 环境对齐，不是独立历史 DOS 证明。可执行的原 EXE 启动前缀入口：

```sh
: "${TMPDIR:?set TMPDIR to a private scratch directory}"
oracle_dir=$(mktemp -d "$TMPDIR/pal-oracle.XXXXXX")
python3 tools/oracle_trace.py --exe assets/unpacked/PAL.raw.exe \
  --manifest docs/resource-manifest.json --stop 33c4:04c0 --max-instructions 100000 \
  --trace "$oracle_dir/oracle.jsonl" --memory "$oracle_dir/oracle.bin" --state "$oracle_dir/oracle.json"
```

输出父目录须已存在、位于 TMPDIR 或仓库 generated/，文件 exclusive-create。stop 是加载后的绝对 CS:IP，执行前停止。
退出 0 为 stop/guest_exit，1 为执行失败/预算/超时，2 为设置失败；guest exit_code 单独记录。
`--memory-input` 要精确 1 MiB 并配 `--initial-state`，只能用于 leaf fixture，不是完整 DOS checkpoint。

[compare_traces.py](../tools/compare_traces.py) 比较 oracle architectural rows 与 native pre-step trace：
仅跳过同 CS:IP 的 REP 中间行，比较每个寄存器/defined FLAGS 和完整 1 MiB RAM；空、不完整、短 trace 或额外 suffix 拒绝。
原 EXE oracle 前缀成功**不自动等于 native 差分通过**：两端 loader/BDA 等初始化必须先对齐，不能忽略 RAM 差异。
给定同环境产生的两个输出目录：

```sh
: "${ORACLE_DIR:?set ORACLE_DIR to oracle outputs}"
: "${NATIVE_DIR:?set NATIVE_DIR to matching native outputs}"
python3 tools/compare_traces.py --oracle-trace "$ORACLE_DIR/oracle.jsonl" \
  --native-trace "$NATIVE_DIR/native.jsonl" --oracle-memory "$ORACLE_DIR/oracle.bin" \
  --native-memory "$NATIVE_DIR/native.bin"
```

[dev_native.py](../tools/dev_native.py) 供有界 headless 构建/探索；新观察只进入下一轮构建，不能作运行回退。
其回归与 oracle/trace 回归位于 `tests/test_dev_native.py`、`tests/test_oracle_trace.py`、`tests/test_compare_traces.py`。

工具代码许可 [GPL-3.0-or-later](../LICENSE)；原资源、存档、生成代码/ELF、WAV、截图和 RAM 不因此获得再分发许可。
