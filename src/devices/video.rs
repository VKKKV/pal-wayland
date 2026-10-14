// SPDX-License-Identifier: GPL-3.0-or-later
//! Mode-13h indexed framebuffer and VGA DAC, without windowing or timing.
//! Only explicit palette/data ports are supported. Not a full VGA device.
use crate::semantics::GuestMemory;

pub const WIDTH: usize = 320;
pub const HEIGHT: usize = 200;
pub const FRAME_BYTES: usize = WIDTH * HEIGHT;
pub const VGA_BASE: usize = 0xa0000;

/// Owned mode-13h host snapshot. Indices remain raw; masking and palette lookup
/// happen only at presentation. The expanded palette contains committed triplets.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IndexedFrame {
    pub indices: Vec<u8>,
    pub palette: [[u8; 4]; 256],
    pub pixel_mask: u8,
}
impl IndexedFrame {
    pub fn validate(&self) -> Result<(), String> {
        if self.indices.len() != FRAME_BYTES {
            return Err("invalid mode-13h framebuffer dimensions".into());
        }
        if self.palette.iter().any(|color| color[3] != 255) {
            return Err("mode-13h palette must be opaque".into());
        }
        Ok(())
    }
    /// CPU compatibility: softbuffer-compatible 0x00RRGGBB pixels.
    pub fn rgb_pixels(&self) -> Result<Vec<u32>, String> {
        self.validate()?;
        let lookup = self
            .palette
            .map(|[r, g, b, _]| (u32::from(r) << 16) | (u32::from(g) << 8) | u32::from(b));
        Ok(self
            .indices
            .iter()
            .map(|&index| lookup[usize::from(index & self.pixel_mask)])
            .collect())
    }
}

/// Host-only publication state. Only a single guarded, contiguous full-screen
/// REP transaction is atomic; unrelated writes and row reveals remain visible.
#[derive(Debug, Default)]
pub struct FramePublication {
    complete: Option<IndexedFrame>,
    active: Option<(u16, u16)>,
}
impl FramePublication {
    pub fn begin(&mut self, at: (u16, u16), before: IndexedFrame) {
        if self.active.is_none() {
            self.complete = Some(before);
            self.active = Some(at);
        }
    }
    pub fn finish(&mut self, at: (u16, u16)) {
        if self.active == Some(at) {
            self.complete = None;
            self.active = None;
        }
    }
    pub fn active(&self) -> bool {
        self.active.is_some()
    }
    pub fn indexed_frame(
        &mut self,
        current: impl FnOnce() -> Result<IndexedFrame, String>,
    ) -> Result<IndexedFrame, String> {
        if self.active.is_some() {
            return self.complete.clone().ok_or("missing complete frame".into());
        }
        // A transaction always supplies a fresh before snapshot in begin().
        // Ordinary redraws need no second owned copy of the current frame.
        current()
    }
}

/// Identity comes only from a statically verified, exactly guarded cycle.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PaletteOwner {
    pub cs: u16,
    pub out_ip: u16,
    pub loop_ip: u16,
}
#[derive(Debug, Clone, Copy)]
struct PaletteStream {
    owner: PaletteOwner,
    remaining: u16,
}

#[derive(Debug, Clone)]
pub struct Dac {
    colors: [[u8; 3]; 256],
    complete_colors: [[u8; 3]; 256],
    palette_stream: Option<PaletteStream>,
    read_index: u8,
    write_index: u8,
    read_component: usize,
    write_component: usize,
    pixel_mask: u8,
}
impl Default for Dac {
    /// Deterministic initial black palette, not a claim about BIOS defaults.
    fn default() -> Self {
        Self {
            colors: [[0; 3]; 256],
            complete_colors: [[0; 3]; 256],
            palette_stream: None,
            read_index: 0,
            write_index: 0,
            read_component: 0,
            write_component: 0,
            pixel_mask: 0xff,
        }
    }
}
impl Dac {
    /// Release only triplets completed by the owned pass. Raw components and
    /// indices are untouched, including a partially written current color.
    fn release_palette_stream(&mut self) {
        if let Some(stream) = self.palette_stream.take() {
            let end = usize::from((768 - stream.remaining) / 3);
            self.complete_colors[..end].copy_from_slice(&self.colors[..end]);
        }
    }
    /// AOT-proven OUT, with a bounded monotonic CX contract. This is not a
    /// general DAC mode: unowned writes always release it before proceeding.
    pub fn write_palette_stream(
        &mut self,
        owner: PaletteOwner,
        count: u16,
        value: u8,
    ) -> Result<(), String> {
        if let Some(stream) = self.palette_stream {
            if stream.owner != owner || stream.remaining != count {
                self.release_palette_stream();
            }
        } else if count == 768 && self.write_index == 0 && self.write_component == 0 {
            self.palette_stream = Some(PaletteStream {
                owner,
                remaining: count,
            });
        }
        self.write_port_inner(0x3c9, value)?;
        if let Some(stream) = &mut self.palette_stream {
            stream.remaining -= 1;
            if stream.remaining == 0 {
                self.release_palette_stream();
            }
        }
        Ok(())
    }
    /// Called after the paired guarded LOOP. An IRQ merely suspends the pass;
    /// a changed counter or legal early exit releases completed triplets.
    pub fn finish_palette_stream(&mut self, owner: PaletteOwner, count: u16, continued: bool) {
        if self
            .palette_stream
            .is_some_and(|s| s.owner == owner && (!continued || s.remaining != count))
        {
            self.release_palette_stream();
        }
    }
    pub fn write_port(&mut self, port: u16, value: u8) -> Result<(), String> {
        if (0x3c6..=0x3c9).contains(&port) {
            self.release_palette_stream();
        }
        self.write_port_inner(port, value)
    }
    fn write_port_inner(&mut self, port: u16, value: u8) -> Result<(), String> {
        match port {
            0x3c6 => self.pixel_mask = value,
            0x3c7 => {
                self.read_index = value;
                self.read_component = 0;
            }
            0x3c8 => {
                self.write_index = value;
                self.write_component = 0;
            }
            0x3c9 => {
                self.colors[usize::from(self.write_index)][self.write_component] = value & 63;
                self.write_component += 1;
                if self.write_component == 3 {
                    if self.palette_stream.is_none() {
                        self.complete_colors[usize::from(self.write_index)] =
                            self.colors[usize::from(self.write_index)];
                    }
                    self.write_component = 0;
                    self.write_index = self.write_index.wrapping_add(1);
                }
            }
            _ => return Err(format!("unsupported VGA DAC output port {port:04x}")),
        }
        Ok(())
    }
    pub fn read_port(&mut self, port: u16) -> Result<u8, String> {
        match port {
            0x3c6 => Ok(self.pixel_mask),
            0x3c8 => Ok(self.write_index),
            0x3c9 => {
                let component = self.colors[usize::from(self.read_index)][self.read_component];
                self.read_component += 1;
                if self.read_component == 3 {
                    self.read_component = 0;
                    self.read_index = self.read_index.wrapping_add(1);
                }
                Ok(component)
            }
            // DAC state readback and interleaving behavior vary by chipset;
            // fail until the actual game's requirements establish a model.
            _ => Err(format!("unsupported VGA DAC input port {port:04x}")),
        }
    }
    pub fn rgb(&self, index: u8) -> [u8; 3] {
        self.colors[usize::from(index & self.pixel_mask)].map(|v| (v << 2) | (v >> 4))
    }
    /// A host snapshot never publishes a half-written RGB triplet. Raw DAC
    /// reads and diagnostic `frame` still expose the real guest device state.
    pub fn presentation_frame(&self, memory: &GuestMemory) -> Result<Vec<u32>, String> {
        self.presentation_indexed_frame(memory)?.rgb_pixels()
    }
    pub fn presentation_indexed_frame(&self, memory: &GuestMemory) -> Result<IndexedFrame, String> {
        let indices = memory
            .as_bytes()
            .get(VGA_BASE..VGA_BASE + FRAME_BYTES)
            .ok_or("guest memory has no complete mode-13h framebuffer")?
            .to_vec();
        let palette = self.complete_colors.map(|color| {
            let [r, g, b] = color.map(|v| (v << 2) | (v >> 4));
            [r, g, b, 255]
        });
        Ok(IndexedFrame {
            indices,
            palette,
            pixel_mask: self.pixel_mask,
        })
    }
    /// Softbuffer-compatible 0x00RRGGBB pixels, reading guest physical VRAM.
    pub fn frame(&self, memory: &GuestMemory) -> Result<Vec<u32>, String> {
        self.frame_with_palette(memory, &self.colors)
    }
    fn frame_with_palette(
        &self,
        memory: &GuestMemory,
        palette: &[[u8; 3]; 256],
    ) -> Result<Vec<u32>, String> {
        let bytes = memory
            .as_bytes()
            .get(VGA_BASE..VGA_BASE + FRAME_BYTES)
            .ok_or("guest memory has no complete mode-13h framebuffer")?;
        // Rebuild a small stack LUT from the selected raw/committed palette.
        // No persistent cache or dirty invalidation touches DAC write semantics.
        let lookup = palette.map(|color| {
            let [r, g, b] = color.map(|v| (v << 2) | (v >> 4));
            (u32::from(r) << 16) | (u32::from(g) << 8) | u32::from(b)
        });
        Ok(bytes
            .iter()
            .map(|&index| lookup[usize::from(index & self.pixel_mask)])
            .collect())
    }
}

/// Nearest-neighbor letterboxing, using integer scaling when the destination
/// can fit a native frame. No guest tick or memory change is tied to rendering.
pub fn scale_frame(source: &[u32], width: usize, height: usize) -> Result<Vec<u32>, String> {
    if source.len() != FRAME_BYTES || width == 0 || height == 0 {
        return Err("invalid framebuffer dimensions".into());
    }
    let pixels = width
        .checked_mul(height)
        .filter(|&n| n <= 64 * 1024 * 1024)
        .ok_or("destination framebuffer too large")?;
    let integer_scale = (width / WIDTH).min(height / HEIGHT);
    let (draw_width, draw_height) = if integer_scale >= 1 {
        (WIDTH * integer_scale, HEIGHT * integer_scale)
    } else if width * HEIGHT <= height * WIDTH {
        (width, (width * HEIGHT / WIDTH).max(1))
    } else {
        ((height * WIDTH / HEIGHT).max(1), height)
    };
    let left = (width - draw_width) / 2;
    let top = (height - draw_height) / 2;
    let mut output = vec![0; pixels];
    for y in 0..draw_height {
        for x in 0..draw_width {
            output[(top + y) * width + left + x] =
                source[(y * HEIGHT / draw_height) * WIDTH + (x * WIDTH / draw_width)];
        }
    }
    Ok(output)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::semantics::{A20, SegmentedAddress};
    const OWNER: PaletteOwner = PaletteOwner {
        cs: 0x1000,
        out_ip: 1,
        loop_ip: 2,
    };
    fn owned(dac: &mut Dac, values: &[u8]) {
        for (i, &v) in values.iter().enumerate() {
            dac.write_palette_stream(OWNER, 768 - i as u16, v).unwrap();
            dac.finish_palette_stream(OWNER, 767 - i as u16, i != 767);
        }
    }
    #[test]
    fn indexed_frame_validation_requires_native_dimensions_and_opaque_palette() {
        let mut frame = IndexedFrame {
            indices: vec![255; FRAME_BYTES],
            palette: [[0, 0, 0, 255]; 256],
            pixel_mask: 0,
        };
        frame.palette[0] = [255, 130, 0, 255];
        frame.validate().unwrap();
        assert!(frame.rgb_pixels().unwrap().iter().all(|&p| p == 0xff8200));
        frame.indices.pop();
        assert!(frame.validate().is_err());
        assert!(frame.rgb_pixels().is_err());
        frame.indices.push(255);
        frame.palette[255][3] = 0;
        assert!(frame.validate().is_err());
        assert!(frame.rgb_pixels().is_err());
    }
    #[test]
    fn palette_owned_pass_is_bounded_and_preserves_raw_dac() {
        let mut dac = Dac::default();
        let mut raw = Dac::default();
        for i in 0..768_u16 {
            let v = i as u8;
            dac.write_palette_stream(OWNER, 768 - i, v).unwrap();
            raw.write_port(0x3c9, v).unwrap();
            assert_eq!(dac.colors, raw.colors);
            assert_eq!(dac.write_index, raw.write_index);
            assert_eq!(dac.write_component, raw.write_component);
            if i < 767 {
                assert_eq!(dac.complete_colors, [[0; 3]; 256]);
            }
            dac.finish_palette_stream(OWNER, 767 - i, i != 767);
        }
        assert!(dac.palette_stream.is_none());
        assert_eq!(dac.complete_colors, raw.colors);
        dac.write_port(0x3c7, 0).unwrap();
        raw.write_port(0x3c7, 0).unwrap();
        for _ in 0..768 {
            assert_eq!(dac.read_port(0x3c9), raw.read_port(0x3c9));
        }
    }
    #[test]
    fn palette_early_exit_and_counter_mismatch_publish_only_complete_triplets() {
        for count in [0, 1, 12, 768, u16::MAX] {
            let mut dac = Dac::default();
            owned(&mut dac, &[63, 0, 0, 63]);
            assert_eq!(dac.complete_colors[0], [0; 3]);
            dac.finish_palette_stream(OWNER, count, count != 0);
            assert!(dac.palette_stream.is_none());
            assert_eq!(dac.complete_colors[0], [63, 0, 0]);
            assert_eq!(dac.complete_colors[1], [0; 3]);
            for v in [63, 0] {
                dac.write_port(0x3c9, v).unwrap();
            }
            assert_eq!(dac.complete_colors[1], [63, 63, 0]);
        }
        // CX mismatch at OUT is detected before writing another component.
        let mut dac = Dac::default();
        owned(&mut dac, &[63, 0, 0]);
        dac.write_palette_stream(OWNER, 1, 63).unwrap();
        assert!(dac.palette_stream.is_none());
        assert_eq!(dac.complete_colors[0], [63, 0, 0]);
        assert_eq!(dac.complete_colors[1], [0; 3]);
    }
    #[test]
    fn foreign_dac_access_and_owner_change_do_not_freeze_publication() {
        for (port, value) in [(0x3c6, 255), (0x3c7, 0), (0x3c8, 7), (0x3c9, 0)] {
            let mut dac = Dac::default();
            owned(&mut dac, &[63, 0, 0, 63]);
            dac.write_port(port, value).unwrap();
            assert!(dac.palette_stream.is_none());
            assert_eq!(dac.complete_colors[0], [63, 0, 0]);
            assert_eq!(dac.complete_colors[1], [0; 3]);
        }
        let mut dac = Dac::default();
        owned(&mut dac, &[63, 0, 0]);
        dac.write_palette_stream(
            PaletteOwner {
                cs: OWNER.cs + 1,
                ..OWNER
            },
            765,
            63,
        )
        .unwrap();
        assert!(dac.palette_stream.is_none());
        assert_eq!(dac.complete_colors[0], [63, 0, 0]);
        // Ordinary short loops never open full-palette atomic publication.
        let mut dac = Dac::default();
        for (count, v) in [(3, 63), (2, 0), (1, 0)] {
            dac.write_palette_stream(OWNER, count, v).unwrap();
        }
        assert!(dac.palette_stream.is_none());
        assert_eq!(dac.complete_colors[0], [63, 0, 0]);
    }
    #[test]
    fn palette_component_order_index_wrap_mask_and_six_bit_expansion() {
        let mut dac = Dac::default();
        dac.write_port(0x3c8, 255).unwrap();
        for v in [63, 32, 0, 0, 0, 63] {
            dac.write_port(0x3c9, v).unwrap();
        }
        assert_eq!(dac.rgb(255), [255, 130, 0]);
        assert_eq!(dac.rgb(0), [0, 0, 255]);
        assert_eq!(dac.read_port(0x3c8).unwrap(), 1);
        dac.write_port(0x3c7, 255).unwrap();
        for v in [63, 32, 0, 0, 0, 63] {
            assert_eq!(dac.read_port(0x3c9).unwrap(), v);
        }
        dac.write_port(0x3c6, 0).unwrap();
        assert_eq!(dac.rgb(255), dac.rgb(0));
        assert!(dac.read_port(0x3da).is_err());
        assert!(dac.write_port(0x3d4, 1).is_err());
    }
    #[test]
    fn frame_reads_real_guest_vram_without_changing_guest_state() {
        let mut memory = GuestMemory::new(0x100000, A20::Disabled);
        let mut dac = Dac::default();
        dac.write_port(0x3c8, 7).unwrap();
        for v in [63, 0, 0] {
            dac.write_port(0x3c9, v).unwrap();
        }
        memory.write8(SegmentedAddress::new(0xa000, 0), 7).unwrap();
        memory
            .write8(SegmentedAddress::new(0xa000, 63999), 7)
            .unwrap();
        let before = memory.clone();
        let frame = dac.frame(&memory).unwrap();
        assert_eq!(frame.len(), 64000);
        assert_eq!(frame[0], 0xff0000);
        assert_eq!(frame[63999], 0xff0000);
        assert_eq!(frame[1], 0);
        assert_eq!(memory, before);
        assert!(dac.frame(&GuestMemory::new(1, A20::Disabled)).is_err());
        // All indices with ordinary masks, raw half-triplets, committed owned
        // streams and early release must match the original scalar formula.
        let indices: Vec<u8> = (0..FRAME_BYTES).map(|i| i as u8).collect();
        memory
            .write_bytes(SegmentedAddress::new(0xa000, 0), &indices)
            .unwrap();
        let check = |dac: &Dac| {
            let before = memory.clone();
            let indexed = dac.presentation_indexed_frame(&memory).unwrap();
            indexed.validate().unwrap();
            assert_eq!(indexed.indices, indices);
            assert_eq!(indexed.pixel_mask, dac.pixel_mask);
            assert_eq!(
                indexed.palette,
                dac.complete_colors.map(|color| {
                    let [r, g, b] = color.map(|v| (v << 2) | (v >> 4));
                    [r, g, b, 255]
                })
            );
            assert_eq!(
                indexed.rgb_pixels().unwrap(),
                dac.presentation_frame(&memory).unwrap()
            );
            assert_eq!(memory, before);
            for (palette, actual) in [
                (&dac.colors, dac.frame(&memory).unwrap()),
                (
                    &dac.complete_colors,
                    dac.presentation_frame(&memory).unwrap(),
                ),
            ] {
                let expected: Vec<u32> = indices
                    .iter()
                    .map(|&index| {
                        let [r, g, b] = palette[usize::from(index & dac.pixel_mask)]
                            .map(|v| (v << 2) | (v >> 4));
                        (u32::from(r) << 16) | (u32::from(g) << 8) | u32::from(b)
                    })
                    .collect();
                assert_eq!(actual, expected);
            }
        };
        dac.write_port(0x3c8, 0).unwrap();
        for i in 0..768 {
            dac.write_port(0x3c9, (i as u8).wrapping_mul(11)).unwrap();
        }
        for mask in [0xff, 0, 0x55, 0x81] {
            dac.write_port(0x3c6, mask).unwrap();
            check(&dac);
        }
        dac.write_port(0x3c6, 0xff).unwrap();
        dac.write_port(0x3c8, 0).unwrap();
        dac.write_port(0x3c9, 63).unwrap();
        check(&dac);
        dac.write_port(0x3c8, 0).unwrap();
        owned(&mut dac, &[63, 0, 0, 63]);
        check(&dac);
        dac.finish_palette_stream(OWNER, 0, false);
        check(&dac);
        dac.write_port(0x3c8, 0).unwrap();
        owned(&mut dac, &vec![32; 768]);
        check(&dac);
    }
    #[test]
    fn scaling_is_bounded_centered_and_preserves_source_pixels() {
        let mut source = vec![0xffffff; FRAME_BYTES];
        source[0] = 0x123456;
        let output = scale_frame(&source, 800, 600).unwrap();
        assert_eq!(output[100 * 800 + 80], 0x123456);
        assert_eq!(output[101 * 800 + 81], 0x123456);
        assert_eq!(output[99 * 800 + 80], 0);
        assert_eq!(output[100 * 800 + 79], 0);
        assert_eq!(scale_frame(&source, 160, 100).unwrap()[0], 0x123456);
        assert_eq!(scale_frame(&source, 1, 1).unwrap(), vec![0x123456]);
        assert!(scale_frame(&source, usize::MAX, 2).is_err());
        assert!(scale_frame(&source, 0, 1).is_err());
    }
}
