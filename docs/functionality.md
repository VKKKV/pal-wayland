# 当前功能账本

固定要求见 [PLAN](PLAN.md)，构建与维护见[开发说明](development.md)。

## 构建与版本

- build 输出 `native_elf` 与 `build_bundle`；直接运行前者，具体路径不写入文档。
- 构建使用 schema 2、player opt-level=3、window；精确源码/ELF 身份由 bundle 内 `generation.json` 记录，装配身份由 `assembly.json` 记录。
- ELF 是冻结产物，不会随源码修改更新；既有产物使用修复需重新构建。
- 键位为 `[keymap]` 的物理宿主键 → DOS 键值对，无 preset；默认路径仅使用 `pal-wayland`，不自动迁移旧配置/存档。

## 已实现与验证边界

| 能力 | 当前状态 |
| --- | --- |
| 静态 AOT | 原 DOS 指令构建期生成 Rust；运行时无 decoder/JIT/CPU fallback，未知目标/服务/代码变化硬失败。 |
| 启动/配置/存档 | ELF-relative 资源；独立 COW saves；默认 Wayland/PipeWire/SB/IRQ。help/controls/print-config 不启动设备或写用户数据；seed 仅播种缺失设置。 |
| 键位/热加载 | 显式键值对、default/user 来源；无配置默认 WASD＋方向键，内置键位和初始化共用编译嵌入的 TOML 示例；其他默认保留启动模式语义。键位/音量/静音热加载，无效修改保留 last-valid；失焦和改映射释放 holds。 |
| 游戏链路 | 标题、新游戏、移动、对话、场景/剧情/物品已有实现及此前执行检查；战斗代表性闭环尚待验证。 |
| 存读档/INSTALL | 原程序存档/重启读取及原 INSTALL UI 配置已有实现；不承诺全部存档点或 mode12 窗口支持。 |
| 显示 | Wayland-only，默认 wgpu/Vulkan，softbuffer 显式选择；索引帧、committed palette/mask 与 REP/DAC 发布契约保持。 |
| 声音 | MSCDEX/CD-DA、VOC→XMS→DMA→SB/IRQ7、单一混音消费及进程内 PipeWire 已接入；提交帧计数不代表可听验收。 |
| 调度/profiling | 有界执行 slice 与呈现分离，PCM 恢复保留时间债；profiling 为编译/运行 opt-in，不测物理显示延迟。 |

源码回归覆盖配置初始化失败/重试、严格类型及 DOS 驱动器/访问模式边界；不代替现场输入、音频与玩法验收。

## 待验证与限制

- 当前版本桌面输入、可听声音和代表性战斗闭环待现场确认；未完成完整通关、全剧情/战斗/存档点或长期兼容验证。
- 战斗需要可确认的附近存档，不再重复长盲前缀；普通动作、敌方回合及返回场景分别验证。
- mode12 planar 服务已实现，窗口尚不支持其 640×480 尺寸；mode3 窗口为启动黑帧，不是文本渲染。
- FM/OPL/MIDI、DOS/BIOS/PIC/VGA/x87 覆盖有限；不是通用 DOS 模拟器，不承诺任意自修改代码。
- 预算退出不等于正常游戏退出；组件测试和像素检查不证明游戏 FPS 或显示延迟改善。

## 证据定位

- 构建身份与阶段日志：以 build 输出的 `generation_provenance` 及所选 bundle 内记录为准。
- 资源身份：[资源 manifest](resource-manifest.json)、[CD manifest](cd-audio-manifest.json)。

根目录 `analysis/` 是 Git 忽略的私有调查材料，不是构建依赖；其中历史记录不代表当前入口。
