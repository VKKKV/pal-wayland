# pal-wayland

将原版《仙剑奇侠传》DOS 程序静态翻译为 Linux 本机 ELF，使用 Wayland 显示、PipeWire 输出声音。

实验性源码项目，仅支持清单中固定哈希的输入；用户自备合法资源，不提供可直接下载的游戏包。当前玩法与现场验证边界见[功能账本](docs/functionality.md)。

## 构建与启动

需要 Rust ≥ 1.97、Python ≥ 3.11、linker、pkg-config、libclang 和 PipeWire 开发头文件，以及 offline Cargo 依赖缓存；运行需要 Wayland、Vulkan GPU 驱动、libxkbcommon 与 PipeWire。

按[资源准备](docs/development.md#原资源准备)放置合法取得且匹配哈希的 `assets/unpacked/PAL.raw.exe`、`assets/dos-cd/`、`assets/GC050.BIN` 和 `.cue`；本仓库不提供原资源。在仓库根缓存依赖并构建：

```sh
cargo fetch --locked
python3 tools/verify_assets.py --runtime
python3 tools/native_game.py build --profile player --build-timeout 1800
```

将成功输出的 `native_elf` 路径赋给 `PAL_ELF`，后续命令使用这个变量：

```sh
: "${PAL_ELF:?set PAL_ELF to the native_elf printed by build}"
"$PAL_ELF" --help
"$PAL_ELF" --print-config
"$PAL_ELF"
```

省略 `--output` 自动生成唯一目录；显式指定时，输出目录及同名缓存必须尚不存在。build 只生成/编译，不启动游戏；`--help` / `--print-config` 不启动设备，直接运行默认打开窗口和声音。

`player` 使用 opt-level=3，`dev` 使用 opt-level=1；两者保留 overflow checks、debug assertions 和代码 guards。仅构建仓库根 crate 不会生成完整游戏 ELF，详细机制见[开发说明](docs/development.md#构建与运行)。

ELF 是冻结产物，不会随源码修改更新；使用源码修复需重新构建。版本与验收边界见[功能账本](docs/functionality.md)。

## 设置

配置为 `$XDG_CONFIG_HOME/pal-wayland/config.toml`（未设时 `~/.config/pal-wayland/config.toml`），存档为 `$XDG_DATA_HOME/pal-wayland/saves`（未设时 `~/.local/share/pal-wayland/saves`）。显式 `--config`/`--saves` 优先；不自动迁移或覆盖已有文件。

默认 WASD 移动，方向键也可用，直接运行即可，无需创建配置文件。需要自定义时，可选用 `--init-config` 创建[完整示例](examples/config.toml)；已有配置不要重复初始化：

```sh
: "${PAL_ELF:?set PAL_ELF to the native_elf printed by build}"
"$PAL_ELF" --init-config
"$PAL_ELF" --controls
"$PAL_ELF" --print-config
```

以下是完整最小配置；编辑已有文件时只修改现有 `[keymap]` 的键值，不要追加重复表：

```toml
version = 1

[keymap]
KeyW = "ArrowUp"
KeyA = "ArrowLeft"
KeyS = "ArrowDown"
KeyD = "ArrowRight"
```

左侧是按 QWERTY 物理位置命名的宿主键，右侧是 DOS 键；未列出的键保留默认，`"None"` 禁用单键。键位、音量、静音支持热加载；改键后松开再按，renderer/音频后端/路径需重启。自定义布局、覆盖来源及旧配置升级见[配置说明](docs/development.md#toml-配置与热加载)。

## 项目架构

项目采用 AOT（ahead-of-time）静态翻译，不是游戏引擎重写。原程序的 x86 指令在构建期转换为 Rust，再由 Cargo 编译为本机 ELF；运行时保留客体状态和必要的 DOS/设备服务，但不解码指令，也没有 JIT 或 CPU 解释器 fallback。

```text
构建期：原 DOS EXE + 已确认入口/跳转表
          → MZ 加载与重定位 → 控制流恢复 → 生成 Rust → Cargo → 本机 ELF
运行时：宿主启动/事件循环 → translated::step → Machine / 指令语义 / 客体设备
          键盘输入 → 客体键盘状态；客体画面/PCM → Wayland / PipeWire
```

源码保持单个 Rust crate，按职责分组：

| 层次                 | 位置                                                  | 职责                                                                                                    |
| -------------------- | ----------------------------------------------------- | ------------------------------------------------------------------------------------------------------- |
| 加载与核心语义       | `src/mz.rs`、`src/semantics.rs`、`src/fpu.rs`         | 校验并重定位 MZ；定义寄存器、FLAGS、分段内存和整数/浮点语义                                             |
| 构建期分析           | `src/analysis/`、`src/bin/pal-wayland-lift.rs`        | 用 iced-x86 解码已确认的代码，恢复控制流并生成 `translated.rs`，保留原 CS:IP 与代码字节 guards          |
| 运行时协调与 DOS ABI | `src/runtime.rs`、`src/runtime/dos.rs`                | `Machine` 持有客体状态和中断/设备路由；私有 DOS 模块处理 INT 21h 参数、服务与寄存器结果；不负责动态解码 |
| 客体设备与服务       | `src/devices/`                                        | VGA/调色板、Sound Blaster/DMA、CD-ROM/CD-DA、XMS，以及只读资源与独立 COW 存档                           |
| 宿主适配             | `src/host/`                                           | 启动参数与 TOML、键位/热加载、Wayland 事件循环、wgpu/Vulkan 显示、进程内 PipeWire 输出和诊断捕获        |
| 构建与运行入口       | `tools/native_game.py`、`templates/native_main.rs.in` | 按 `docs/native-target.json` 生成/编译并记录产物身份；模板连接静态代码与宿主，直接运行 bundle 的 ELF    |

职责上分为“转换工具 + 客体服务运行库 + 宿主适配”，而非 relinker + PRX。转换器、DOS/设备服务和宿主 I/O 的边界及其与 AnyPS5 的区别见[架构契约](docs/development.md#架构职责契约)。

`analysis` feature 仅供构建期工具使用；生成项目以 `default-features = false` 链接运行库，不带 iced-x86 decoder。`window` 启用 Wayland 显示，默认 renderer 为 wgpu/Vulkan，softbuffer 必须显式选择，无 X11 或初始化失败后的静默回退。未知静态目标、未支持服务或未经许可的代码变化会明确停止。

`assets/` 保存本地原资源，`generated/` 保存私有构建产物；`target/` 保存可重建编译缓存。更详细的构建、运行与测试契约见[开发说明](docs/development.md)。

## 文档与许可

- [当前功能、限制与构建身份](docs/functionality.md)
- [开发、构建与测试](docs/development.md)
- [固定目标与验收要求](docs/PLAN.md)

代码采用 [GPL-3.0-or-later](LICENSE)；[第三方与游戏资源](docs/dependency-notices.md)遵各自许可，原资源及派生代码/ELF 不纳入公开交付。
