// SPDX-License-Identifier: GPL-3.0-or-later
//! Minimal BIOS mode-03h, page-0 color text semantics (80 columns x 25 rows).
//!
//! Guest B800:0000..0F9F is the only cell storage. Temporary stack frames below
//! are per-call transactions, never retained display/shadow state. Direct guest
//! writes are visible immediately. No decoder, renderer, font, or speaker.
//!
//! Runtime integration (the caller owns mode, cursor and BIOS data area):
//! - On clearing mode 03h, call `initialize(memory)` before publishing the mode
//!   or resetting the cursor to 0000h. AL bit 7 (preserve VRAM) must skip it.
//! - AH=02h: validate mode/page and `validate_cursor(DX)` before publishing DX.
//! - AH=08h: `AX = read_cell(memory, mode, BH, cursor)?` (AH=attr, AL=char).
//! - AH=09h/0Ah: `write_repeat(memory, mode, BH, cursor, AL, Some(BL)/None, CX)`;
//!   leave the cursor unchanged. The returned count reports screen-end clipping.
//! - AH=06h/07h: `scroll(memory, mode, active_page, Rect { top: CH, left: CL,
//!   bottom: DH, right: DL }, AL, BH, Up/Down)`; BH is an ATTRIBUTE, not page.
//! - AH=0Eh: `teletype(memory, mode, BH, &mut cursor, AL)`; BL is ignored in
//!   text mode. `BellIgnored` explicitly reports the unsupported sound device.
//! - Mirror cursor into BDA 0040:0050 only after success. If composing the helper
//!   with a fallible BDA write, preflight that write BEFORE calling the helper
//!   to preserve the larger runtime transaction (the helper does not own BDA).
//!
//! Only mode 3/page 0 is accepted; there is no fake multipage compatibility.
//! Every VRAM operation requires the complete 4000-byte screen even for no-ops.
//! Invalid cursors/rectangles are rejected, not clamped. Repeats wrap rows but
//! clip at screen end without scrolling; scroll counts saturate at window height.
//! These edge policies are explicit bounded-host behavior, not claims about
//! out-of-range inputs on all historical BIOSes. Returned errors preserve all
//! guest bytes and caller cursor. Allocation failure is outside that guarantee.
//!
//! Protocol references: RBIL INT 10h AH=06/07/08/09/0A/0E,
//! <https://www.ctyme.com/intr/rb-0096.htm> through rb-0100.htm and rb-0106.htm.
//! AH=09/0A cursor preservation cross-checked with HelpPC INT 10,9 and 10,A.

#![forbid(unsafe_code)]

use crate::semantics::{GuestMemory, SegmentedAddress};

pub const COLUMNS: usize = 80;
pub const ROWS: usize = 25;
pub const CELLS: usize = COLUMNS * ROWS;
pub const SCREEN_BYTES: usize = CELLS * 2;
pub const VRAM_BASE: usize = 0xb8000;
pub const DEFAULT_ATTRIBUTE: u8 = 0x07;
const VRAM: SegmentedAddress = SegmentedAddress::new(0xb800, 0);

/// Inclusive BIOS CH/CL..DH/DL rectangle, with zero-based coordinates.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Rect {
    pub top: u8,
    pub left: u8,
    pub bottom: u8,
    pub right: u8,
}

impl Rect {
    pub const FULL_SCREEN: Self = Self {
        top: 0,
        left: 0,
        bottom: 24,
        right: 79,
    };

    pub fn validate(self) -> Result<(), String> {
        if self.top > self.bottom
            || self.left > self.right
            || usize::from(self.bottom) >= ROWS
            || usize::from(self.right) >= COLUMNS
        {
            return Err(format!("invalid 80x25 text rectangle {self:?}"));
        }
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ScrollDirection {
    Up,
    Down,
}

/// No bell audio is synthesized or sent to host devices.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TeletypeEffect {
    CharacterWritten,
    CursorMoved,
    BellIgnored,
}

pub fn validate_context(mode: u8, page: u8) -> Result<(), String> {
    if mode != 3 || page != 0 {
        return Err(format!(
            "text video supports only BIOS mode 03h/page 0 (mode={mode:02x}, page={page})"
        ));
    }
    Ok(())
}

/// BIOS DX packing: DH=row, DL=column. Returns the row-major cell index.
pub fn validate_cursor(cursor: u16) -> Result<usize, String> {
    let row = usize::from(cursor >> 8);
    let column = usize::from(cursor & 0xff);
    if row >= ROWS || column >= COLUMNS {
        return Err(format!("invalid 80x25 text cursor {cursor:04x}"));
    }
    Ok(row * COLUMNS + column)
}

fn screen(memory: &GuestMemory) -> Result<&[u8], String> {
    memory
        .as_bytes()
        .get(VRAM_BASE..VRAM_BASE + SCREEN_BYTES)
        .ok_or_else(|| "guest memory has no complete 4000-byte mode-03h text screen".into())
}

fn frame(memory: &GuestMemory) -> Result<[u8; SCREEN_BYTES], String> {
    let mut bytes = [0; SCREEN_BYTES];
    bytes.copy_from_slice(screen(memory)?);
    Ok(bytes)
}

fn commit(memory: &mut GuestMemory, bytes: &[u8; SCREEN_BYTES]) -> Result<(), String> {
    // GuestMemory preflights the entire write. B800 is below the A20 gate and
    // 4000 bytes do not cross the 64-KiB segment limit.
    memory.write_bytes(VRAM, bytes).map_err(|e| e.to_string())
}

/// Clear exactly page 0's visible cells to ASCII space/attribute 07h.
/// Does not change mode, cursor, BDA, page padding or any other memory.
pub fn initialize(memory: &mut GuestMemory) -> Result<(), String> {
    screen(memory)?;
    let mut bytes = [0; SCREEN_BYTES];
    for cell in bytes.chunks_exact_mut(2) {
        cell.copy_from_slice(&[b' ', DEFAULT_ATTRIBUTE]);
    }
    commit(memory, &bytes)
}

/// AH=08h result packed as AX: character in AL, attribute in AH.
pub fn read_cell(memory: &GuestMemory, mode: u8, page: u8, cursor: u16) -> Result<u16, String> {
    validate_context(mode, page)?;
    let offset = validate_cursor(cursor)? * 2;
    let bytes = screen(memory)?;
    Ok(u16::from_le_bytes([bytes[offset], bytes[offset + 1]]))
}

/// AH=09h (`Some(attr)`) / AH=0Ah (`None` preserves EACH cell's attribute).
/// Control bytes are literal glyphs here. Never moves cursor or scrolls.
/// CX=0 is a validated no-op; oversized CX clips, returning cells written.
#[allow(clippy::too_many_arguments)]
pub fn write_repeat(
    memory: &mut GuestMemory,
    mode: u8,
    page: u8,
    cursor: u16,
    ch: u8,
    attr: Option<u8>,
    len: u16,
) -> Result<usize, String> {
    validate_context(mode, page)?;
    let start = validate_cursor(cursor)?;
    let mut bytes = frame(memory)?;
    let count = usize::from(len).min(CELLS - start);
    if count == 0 {
        return Ok(0);
    }
    for index in start..start + count {
        bytes[index * 2] = ch;
        if let Some(value) = attr {
            bytes[index * 2 + 1] = value;
        }
    }
    commit(memory, &bytes)?;
    Ok(count)
}

// Inputs are validated and frame complete before this infallible transaction.
fn scroll_frame(
    bytes: &mut [u8; SCREEN_BYTES],
    rect: Rect,
    lines: u8,
    attr: u8,
    direction: ScrollDirection,
) {
    let top = usize::from(rect.top);
    let bottom = usize::from(rect.bottom);
    let left = usize::from(rect.left);
    let width = usize::from(rect.right) - left + 1;
    let height = bottom - top + 1;
    let count = if lines == 0 {
        height
    } else {
        usize::from(lines).min(height)
    };
    let row_start = |row| (row * COLUMNS + left) * 2;
    match direction {
        ScrollDirection::Up => {
            for row in top..bottom + 1 - count {
                let source = row_start(row + count);
                bytes.copy_within(source..source + width * 2, row_start(row));
            }
        }
        ScrollDirection::Down => {
            for row in (top + count..=bottom).rev() {
                let source = row_start(row - count);
                bytes.copy_within(source..source + width * 2, row_start(row));
            }
        }
    }
    let (blank_start, blank_end) = match direction {
        ScrollDirection::Up => (bottom + 1 - count, bottom),
        ScrollDirection::Down => (top, top + count - 1),
    };
    for row in blank_start..=blank_end {
        let start = row_start(row);
        for cell in bytes[start..start + width * 2].chunks_exact_mut(2) {
            cell.copy_from_slice(&[b' ', attr]);
        }
    }
}

/// AH=06/07h: AL=lines (zero clears), BH=blank attribute, active page only.
/// Retains moved cells' character AND attribute, outside-window bytes and cursor.
#[allow(clippy::too_many_arguments)]
pub fn scroll(
    memory: &mut GuestMemory,
    mode: u8,
    active_page: u8,
    rect: Rect,
    lines: u8,
    attr: u8,
    direction: ScrollDirection,
) -> Result<(), String> {
    validate_context(mode, active_page)?;
    rect.validate()?;
    let mut bytes = frame(memory)?;
    scroll_frame(&mut bytes, rect, lines, attr, direction);
    commit(memory, &bytes)
}

/// AH=0Eh with caller-owned BIOS DX cursor. Preserves existing text attributes.
/// BS moves left without erasing or crossing column zero; CR sets column zero;
/// LF advances row preserving column; BEL returns `BellIgnored` with no changes.
/// Printable bytes wrap at column 79 and scroll at row 24. Scrolling blanks
/// use the attribute at the ORIGINAL cursor cell (also for LF), matching the
/// selected minimal text-device policy. Tab and all other bytes are literal.
/// Both VRAM and cursor remain unchanged on every returned error.
pub fn teletype(
    memory: &mut GuestMemory,
    mode: u8,
    page: u8,
    cursor: &mut u16,
    ch: u8,
) -> Result<TeletypeEffect, String> {
    validate_context(mode, page)?;
    let index = validate_cursor(*cursor)?;
    let mut bytes = frame(memory)?;
    let attr = bytes[index * 2 + 1];
    let mut row = usize::from(*cursor >> 8);
    let mut column = usize::from(*cursor & 0xff);
    let mut changed = false;
    let effect = match ch {
        7 => return Ok(TeletypeEffect::BellIgnored),
        8 => {
            column = column.saturating_sub(1);
            TeletypeEffect::CursorMoved
        }
        13 => {
            column = 0;
            TeletypeEffect::CursorMoved
        }
        10 => {
            row += 1;
            TeletypeEffect::CursorMoved
        }
        _ => {
            bytes[index * 2] = ch;
            changed = true;
            column += 1;
            if column == COLUMNS {
                column = 0;
                row += 1;
            }
            TeletypeEffect::CharacterWritten
        }
    };
    if row == ROWS {
        scroll_frame(&mut bytes, Rect::FULL_SCREEN, 1, attr, ScrollDirection::Up);
        row = ROWS - 1;
        changed = true;
    }
    if changed {
        commit(memory, &bytes)?;
    }
    *cursor = ((row as u16) << 8) | column as u16;
    Ok(effect)
}
