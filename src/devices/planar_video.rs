// SPDX-License-Identifier: GPL-3.0-or-later
//! 有界、std-only 的 mode-12h planar VRAM；不是完整 VGA/BIOS 模拟器。
//!
//! ## 集成契约（本模块不修改 runtime）
//! - `PlanarVideo` 的四个 64-KiB plane 是唯一图形存储；不得向 GuestMemory
//!   A0000 区复制 framebuffer/shadow 来代替设备读写。B8000 文本仍走原内存。
//! - BIOS AL=12h/92h 可调用 `set_mode(AL)`，再提交 runtime 模式/BDA；需要组合
//!   事务时先检查 BDA 的完整写区。bit 7 保留所有 plane，但重置寄存器/latches。
//!   本模块仅支持 640x480x16，其他 planar 模式必须另列尺寸表，不可偷偷映射。
//! - 将 3CE/3CF、3C4/3C5 的 IN/OUT 路由到 `read_port`/`write_port`；16-bit
//!   OUT 必须调用 `write_port16`，低字节=index、高字节=data，失败不改变 index。
//! - CPU read8/16/32 与 write8/16/32 应路由物理 A0000..B0000（右端不含），
//!   READ 必须可修改 latches；原 runtime 的 `read8(&self)` 因而需要调整。
//!   地址转换、segment-limit/A20 与整次混合 RAM/VRAM 请求预检仍由调用者负责。
//! - REP/string、DOS 缓冲区、`memory.read_bytes`/`write_linear` 等绕过 Machine
//!   的入口同样需要统一路由。整段跨出 aperture 必须先拒绝/预检，不能先写一半；
//!   CPU word/dword 是连续字节访问，不是自动跨到下一 plane。
//! - `frame_indices`/`frame` 从 plane 直接生成快照，无副作用；默认 16 色 EGA
//!   palette 是确定的 0x00RRGGBB。AC/DAC/CRTC、字体、时序与翻页未实现；父层可
//!   通过 `frame_with_palette` 提供已解析的 16 项 palette，不得据此声称完整 VGA。
//!
//! 支持 GC 00..08，GC05 仅 read/write-mode bits，GC06 仅 05h（64-KiB A000
//! aperture、graphics、非 odd/even）；SEQ 仅 02(map mask)/04(固定 06h unchained)。
//! 已实现 register 的未使用位写入忽略、读回为 0（确定性模型约定，不保证所有
//! VGA 克隆芯片的保留位读值）；有效位的未实现模式、未知 index/port 仍显式失败。
//! GC 有效 mask 为 `[0f,0f,0f,1f,03,7b,0f,0f,ff]`，SEQ02=0f、SEQ04=0e；
//! 尤其 GC05 的 10/20/40h 是实际 odd/even/shift 控制，绝不能当保留位吞掉。
//! 所有返回 Err 的操作保留整个设备状态及调用者输出缓冲；分配失败不在保证内。
//! reset 后 latches=0 是本模块的确定性初始约定，不是上电硬件状态保证。
//!
//! 行为依据 FreeVGA（J. D. Neal，Stanford 镜像）：
//! <https://web.stanford.edu/class/cs140/projects/pintos/specs/freevga/vga/vgamem.htm>
//! <https://web.stanford.edu/class/cs140/projects/pintos/specs/freevga/vga/graphreg.htm>
//! <https://web.stanford.edu/class/cs140/projects/pintos/specs/freevga/vga/seqreg.htm>
//! 特别：mode3 忽略 logical-op/enable-set-reset；read1 的 don't-care 位为 1
//! 才参与比较。读一次加载全部四个 latch，写入不会刷新 latch。

#![forbid(unsafe_code)]

pub const MODE: u8 = 0x12;
pub const WIDTH: usize = 640;
pub const HEIGHT: usize = 480;
pub const STRIDE: usize = WIDTH / 8;
pub const PLANE_BYTES: usize = 0x10000;
pub const VRAM_BASE: u32 = 0xa0000;
pub const VRAM_END: u32 = 0xb0000;
pub const EGA_PALETTE: [u32; 16] = [
    0x000000, 0x0000aa, 0x00aa00, 0x00aaaa, 0xaa0000, 0xaa00aa, 0xaa5500, 0xaaaaaa, 0x555555,
    0x5555ff, 0x55ff55, 0x55ffff, 0xff5555, 0xff55ff, 0xffff55, 0xffffff,
];

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PlanarVideo {
    // Heap allocation avoids a large fixed array on a guest/runtime stack.
    planes: [Vec<u8>; 4],
    latches: [u8; 4],
    gc: [u8; 9],
    gc_index: u8,
    map_mask: u8,
    seq_index: u8,
}

impl Default for PlanarVideo {
    fn default() -> Self {
        Self {
            planes: std::array::from_fn(|_| vec![0; PLANE_BYTES]),
            latches: [0; 4],
            gc: [0, 0, 0, 0, 0, 0, 5, 15, 255],
            gc_index: 0,
            map_mask: 15,
            seq_index: 2,
        }
    }
}

impl PlanarVideo {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn dimensions(&self) -> (usize, usize) {
        (WIDTH, HEIGHT)
    }

    /// AL bit 7 means preserve VRAM; all registers/latches are reset either way.
    pub fn set_mode(&mut self, al: u8) -> Result<(), String> {
        if al & 0x7f != MODE {
            return Err(format!("unsupported planar BIOS mode {al:02x}"));
        }
        if al & 0x80 == 0 {
            for plane in &mut self.planes {
                plane.fill(0);
            }
        }
        self.latches = [0; 4];
        self.gc = [0, 0, 0, 0, 0, 0, 5, 15, 255];
        self.gc_index = 0;
        self.map_mask = 15;
        self.seq_index = 2;
        Ok(())
    }

    /// Read-only diagnostics, not a second mutable storage path.
    pub fn plane(&self, plane: usize) -> Result<&[u8], String> {
        self.planes
            .get(plane)
            .map(Vec::as_slice)
            .ok_or_else(|| format!("invalid planar plane {plane}"))
    }

    pub fn latches(&self) -> [u8; 4] {
        self.latches
    }

    fn validate_index(port: u16, index: u8) -> Result<(), String> {
        match (port, index) {
            (0x3ce, 0..=8) | (0x3c4, 2 | 4) => Ok(()),
            _ => Err(format!(
                "unsupported planar index port={port:04x} index={index:02x}"
            )),
        }
    }

    /// Mask only documented unused bits, then reject unimplemented *real* fields.
    /// Validate the literal index first: reserved data bits never alias an index.
    fn normalize_data(port: u16, index: u8, value: u8) -> Result<u8, String> {
        Self::validate_index(port, index)?;
        let mask = match (port, index) {
            (0x3ce, 0 | 1 | 2 | 6 | 7) | (0x3c4, 2) => 0x0f,
            (0x3ce, 3) => 0x1f,
            (0x3ce, 4) => 0x03,
            (0x3ce, 5) => 0x7b, // bit 2/7 unused; odd/even and both shifts real.
            (0x3ce, 8) => 0xff,
            (0x3c4, 4) => 0x0e, // bit 0 and bits 4..7 unused, not chain4.
            _ => unreachable!("index validated above"),
        };
        let normalized = value & mask;
        let valid = match (port, index) {
            (0x3ce, 5) => normalized & !0x0b == 0,
            (0x3ce, 6) => normalized == 5,
            (0x3c4, 4) => normalized == 6,
            _ => true,
        };
        if valid {
            Ok(normalized)
        } else {
            Err(format!(
                "unsupported planar register value port={port:04x} index={index:02x} value={value:02x}"
            ))
        }
    }

    // Validation must precede calling this infallible register commit.
    fn commit_data(&mut self, port: u16, index: u8, value: u8) {
        match port {
            0x3ce => self.gc[usize::from(index)] = value,
            0x3c4 if index == 2 => self.map_mask = value,
            _ => {} // SEQ04 can only be its already-fixed 06h value.
        }
    }

    pub fn write_port(&mut self, port: u16, value: u8) -> Result<(), String> {
        match port {
            0x3ce | 0x3c4 => {
                Self::validate_index(port, value)?;
                if port == 0x3ce {
                    self.gc_index = value;
                } else {
                    self.seq_index = value;
                }
            }
            0x3cf | 0x3c5 => {
                let (base, index) = if port == 0x3cf {
                    (0x3ce, self.gc_index)
                } else {
                    (0x3c4, self.seq_index)
                };
                let normalized = Self::normalize_data(base, index, value)?;
                self.commit_data(base, index, normalized);
            }
            _ => return Err(format!("unsupported planar output port {port:04x}")),
        }
        Ok(())
    }

    /// Atomic indexed pair; not two fallible sequential write_port calls.
    pub fn write_port16(&mut self, port: u16, value: u16) -> Result<(), String> {
        let [index, data] = value.to_le_bytes();
        let normalized = Self::normalize_data(port, index, data)?;
        self.commit_data(port, index, normalized);
        if port == 0x3ce {
            self.gc_index = index;
        } else {
            self.seq_index = index;
        }
        Ok(())
    }

    pub fn read_port(&self, port: u16) -> Result<u8, String> {
        match port {
            0x3ce => Ok(self.gc_index),
            0x3cf => Ok(self.gc[usize::from(self.gc_index)]),
            0x3c4 => Ok(self.seq_index),
            0x3c5 => Ok(if self.seq_index == 2 {
                self.map_mask
            } else {
                6
            }),
            _ => Err(format!("unsupported planar input port {port:04x}")),
        }
    }

    /// Complete physical aperture preflight. Empty spans permit the one-past-end
    /// address, but never a start outside A0000..=B0000; no wrapping/plane spill.
    pub fn validate_range(start: u32, len: usize) -> Result<usize, String> {
        if !(VRAM_BASE..=VRAM_END).contains(&start) || len > (VRAM_END - start) as usize {
            return Err(format!(
                "planar access outside A0000..B0000: start={start:x} len={len}"
            ));
        }
        Ok((start - VRAM_BASE) as usize)
    }

    fn read_offset(&mut self, offset: usize) -> u8 {
        for (latch, plane) in self.latches.iter_mut().zip(&self.planes) {
            *latch = plane[offset];
        }
        if self.gc[5] & 8 == 0 {
            return self.latches[usize::from(self.gc[4])];
        }
        let mut result = 0xff;
        for plane in 0..4 {
            if self.gc[7] & (1 << plane) != 0 {
                let compare = if self.gc[2] & (1 << plane) != 0 {
                    0xff
                } else {
                    0
                };
                result &= !(self.latches[plane] ^ compare);
            }
        }
        result
    }

    fn write_offset(&mut self, offset: usize, data: u8) {
        let mode = self.gc[5] & 3;
        let rotated = data.rotate_right(u32::from(self.gc[3] & 7));
        let mask = if mode == 3 {
            self.gc[8] & rotated
        } else {
            self.gc[8]
        };
        for plane in 0..4 {
            if self.map_mask & (1 << plane) == 0 {
                continue;
            }
            let latch = self.latches[plane];
            let result = if mode == 1 {
                latch
            } else {
                let source = match mode {
                    0 if self.gc[1] & (1 << plane) == 0 => rotated,
                    2 => {
                        if data & (1 << plane) != 0 {
                            0xff
                        } else {
                            0
                        }
                    }
                    _ => {
                        if self.gc[0] & (1 << plane) != 0 {
                            0xff
                        } else {
                            0
                        }
                    }
                };
                let operated = if mode == 3 {
                    source
                } else {
                    match (self.gc[3] >> 3) & 3 {
                        0 => source,
                        1 => source & latch,
                        2 => source | latch,
                        _ => source ^ latch,
                    }
                };
                (operated & mask) | (latch & !mask)
            };
            self.planes[plane][offset] = result;
        }
    }

    /// Reads in ascending byte order. Final latches correspond to the last byte.
    pub fn read_linear(&mut self, start: u32, output: &mut [u8]) -> Result<(), String> {
        let offset = Self::validate_range(start, output.len())?;
        for (i, byte) in output.iter_mut().enumerate() {
            *byte = self.read_offset(offset + i);
        }
        Ok(())
    }

    /// Writes never reload latches, including multi-byte transfers.
    pub fn write_linear(&mut self, start: u32, data: &[u8]) -> Result<(), String> {
        let offset = Self::validate_range(start, data.len())?;
        for (i, &byte) in data.iter().enumerate() {
            self.write_offset(offset + i, byte);
        }
        Ok(())
    }

    pub fn read8(&mut self, physical: u32) -> Result<u8, String> {
        let mut data = [0; 1];
        self.read_linear(physical, &mut data)?;
        Ok(data[0])
    }

    pub fn read16(&mut self, physical: u32) -> Result<u16, String> {
        let mut data = [0; 2];
        self.read_linear(physical, &mut data)?;
        Ok(u16::from_le_bytes(data))
    }

    pub fn read32(&mut self, physical: u32) -> Result<u32, String> {
        let mut data = [0; 4];
        self.read_linear(physical, &mut data)?;
        Ok(u32::from_le_bytes(data))
    }

    pub fn write8(&mut self, physical: u32, value: u8) -> Result<(), String> {
        self.write_linear(physical, &[value])
    }

    pub fn write16(&mut self, physical: u32, value: u16) -> Result<(), String> {
        self.write_linear(physical, &value.to_le_bytes())
    }

    pub fn write32(&mut self, physical: u32, value: u32) -> Result<(), String> {
        self.write_linear(physical, &value.to_le_bytes())
    }

    /// Row-major palette indices, bit 7 leftmost; display reads do not load latches.
    pub fn frame_indices(&self) -> Vec<u8> {
        let mut output = vec![0; WIDTH * HEIGHT];
        for (pixel, index) in output.iter_mut().enumerate() {
            let offset = pixel / 8;
            let mask = 0x80 >> (pixel % 8);
            for plane in 0..4 {
                if self.planes[plane][offset] & mask != 0 {
                    *index |= 1 << plane;
                }
            }
        }
        output
    }

    /// Caller supplies resolved 0x00RRGGBB entries (AC/DAC handling is external).
    pub fn frame_with_palette(&self, palette: &[u32; 16]) -> Vec<u32> {
        self.frame_indices()
            .into_iter()
            .map(|index| palette[usize::from(index)])
            .collect()
    }

    pub fn frame(&self) -> Vec<u32> {
        self.frame_with_palette(&EGA_PALETTE)
    }
}
