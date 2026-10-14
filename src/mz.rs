// SPDX-License-Identifier: GPL-3.0-or-later
//! Strict, static DOS MZ container parsing and a deliberately limited loader.
//!
//! Loading does not execute code or construct a DOS process: there is no PSP,
//! environment, interrupt state, DOS memory allocator, or checksum enforcement.
//! The buffer contains the declared image, paragraph padding, and exactly the
//! minimum extra allocation (not the maximum requested allocation). Padding and
//! extra memory are zeroed for this project's deterministic analysis; DOS does
//! **not** guarantee their initial contents. Trailing file bytes are not loaded.
//!
//! Relative CS:IP is interpreted modulo 20 bits, allowing packed negative CS.
//! At load time the actual, relocated CS:IP must equal the expected *unwrapped*
//! physical address. A20/segment-wrap aliases are not silently accepted. Stack
//! segments are restricted to nonnegative image-relative SS. SP=0 denotes a
//! logical stack top at SS:10000h (the next PUSH wraps SP to FFFEh); that complete
//! range must fit the allocation. SP=1, whose first word PUSH wraps, is rejected.
//! Other stack tops may be one byte past the allocation's last byte, and must
//! leave room for a first word PUSH. These checks are not a proof of stack depth.
//!
//! Secondary-header detection recognizes the extended-header convention
//! (e_lfarlc=40h), plus relocation-free stubs with e_lfarlc=0. It requires an
//! in-file e_lfanew >= 40h and the complete format signature; it does not validate
//! an entire NE/LE/LX/PE container. Other old MZ header layouts do not acquire an
//! e_lfanew field merely because their headers happen to extend past offset 3ch.

use std::error::Error;
use std::fmt;

const FIXED_HEADER_BYTES: usize = 28;
const PARAGRAPH_BYTES: usize = 16;
const PAGE_BYTES: usize = 512;
const REAL_MODE_BYTES: usize = 1 << 20;

/// The original fourteen little-endian MZ header words, with Python inspector names.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Header {
    pub magic: u16,
    pub last_page_bytes: u16,
    pub pages: u16,
    pub relocations: u16,
    pub header_paragraphs: u16,
    pub min_extra_paragraphs: u16,
    pub max_extra_paragraphs: u16,
    pub ss: u16,
    pub sp: u16,
    pub checksum: u16,
    pub ip: u16,
    pub cs: u16,
    pub relocation_table_offset: u16,
    pub overlay_number: u16,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Relocation {
    pub offset: u16,
    pub segment: u16,
}

impl Relocation {
    /// Unwrapped offset from the image base, not a 20-bit alias.
    pub fn image_offset(&self) -> usize {
        // Widen first: two u16 inputs cannot exceed 0x10ffef. No host-sized
        // multiplication of untrusted counts or lengths is used here.
        ((u32::from(self.segment) << 4) + u32::from(self.offset)) as usize
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SecondaryHeaderKind {
    Ne,
    Le,
    Lx,
    Pe,
}

impl fmt::Display for SecondaryHeaderKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Ne => "NE",
            Self::Le => "LE",
            Self::Lx => "LX",
            Self::Pe => "PE",
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MzError {
    TruncatedHeader {
        actual_bytes: usize,
    },
    InvalidMagic {
        magic: u16,
    },
    InvalidPageCount,
    InvalidLastPageBytes {
        value: u16,
    },
    HeaderTooSmall {
        header_bytes: usize,
    },
    HeaderBeyondDeclared {
        header_bytes: usize,
        declared_bytes: usize,
    },
    TruncatedFile {
        declared_bytes: usize,
        actual_bytes: usize,
    },
    InvalidExtraParagraphs {
        minimum: u16,
        maximum: u16,
    },
    UnsupportedOverlay {
        number: u16,
    },
    UnsupportedSecondaryHeader {
        kind: SecondaryHeaderKind,
        offset: usize,
    },
    RelocationTableBeforeFixedHeader {
        offset: usize,
    },
    RelocationTableBeyondHeader {
        end: usize,
        header_bytes: usize,
    },
    RelocationTargetOutsideImage {
        index: usize,
        offset: usize,
        image_bytes: usize,
    },
    EntryOutsideImage {
        offset: usize,
        image_bytes: usize,
    },
    MemoryOutsideRealMode {
        load_base: usize,
        memory_bytes: usize,
    },
    StackPointerTooSmall {
        sp: u16,
    },
    StackOutsideMemory {
        offset: usize,
        memory_bytes: usize,
    },
    AddressWrap {
        register: &'static str,
        expected: usize,
        actual: usize,
    },
    ArithmeticOverflow {
        context: &'static str,
    },
    AllocationFailed {
        bytes: usize,
    },
    TruncatedField {
        offset: usize,
        bytes: usize,
    },
}

impl fmt::Display for MzError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::TruncatedHeader { actual_bytes } => {
                write!(f, "MZ header needs 28 bytes; got {actual_bytes}")
            }
            Self::InvalidMagic { magic } => write!(f, "invalid MZ/ZM signature {magic:#06x}"),
            Self::InvalidPageCount => f.write_str("MZ page count must not be zero"),
            Self::InvalidLastPageBytes { value } => {
                write!(f, "last-page bytes must be below 512; got {value}")
            }
            Self::HeaderTooSmall { header_bytes } => write!(
                f,
                "declared MZ header is shorter than 28 bytes: {header_bytes}"
            ),
            Self::HeaderBeyondDeclared {
                header_bytes,
                declared_bytes,
            } => write!(
                f,
                "MZ header ({header_bytes} bytes) exceeds declared file ({declared_bytes} bytes)"
            ),
            Self::TruncatedFile {
                declared_bytes,
                actual_bytes,
            } => write!(
                f,
                "truncated MZ file: declared {declared_bytes} bytes, got {actual_bytes}"
            ),
            Self::InvalidExtraParagraphs { minimum, maximum } => write!(
                f,
                "minimum extra paragraphs {minimum} exceeds maximum {maximum}"
            ),
            Self::UnsupportedOverlay { number } => {
                write!(f, "MZ overlay number {number} is unsupported")
            }
            Self::UnsupportedSecondaryHeader { kind, offset } => write!(
                f,
                "{kind} secondary header at {offset:#x} is not a plain DOS MZ executable"
            ),
            Self::RelocationTableBeforeFixedHeader { offset } => write!(
                f,
                "relocation table at {offset:#x} overlaps the fixed MZ header"
            ),
            Self::RelocationTableBeyondHeader { end, header_bytes } => write!(
                f,
                "relocation table ends at {end:#x}, beyond header size {header_bytes:#x}"
            ),
            Self::RelocationTargetOutsideImage {
                index,
                offset,
                image_bytes,
            } => write!(
                f,
                "relocation {index} word at {offset:#x} is outside {image_bytes:#x}-byte image"
            ),
            Self::EntryOutsideImage {
                offset,
                image_bytes,
            } => write!(
                f,
                "entry offset {offset:#x} is outside {image_bytes:#x}-byte image"
            ),
            Self::MemoryOutsideRealMode {
                load_base,
                memory_bytes,
            } => write!(
                f,
                "allocation at {load_base:#x} of {memory_bytes:#x} bytes exceeds the 1 MiB address space"
            ),
            Self::StackPointerTooSmall { sp } => write!(
                f,
                "SP={sp:#06x} cannot support a non-wrapping first word PUSH"
            ),
            Self::StackOutsideMemory {
                offset,
                memory_bytes,
            } => write!(
                f,
                "logical stack top {offset:#x} is outside {memory_bytes:#x}-byte allocation"
            ),
            Self::AddressWrap {
                register,
                expected,
                actual,
            } => write!(
                f,
                "{register} wrap/alias: expected linear address {expected:#x}, got {actual:#x}"
            ),
            Self::ArithmeticOverflow { context } => {
                write!(f, "arithmetic overflow while computing {context}")
            }
            Self::AllocationFailed { bytes } => {
                write!(f, "cannot allocate {bytes} bytes for MZ loading/parsing")
            }
            Self::TruncatedField { offset, bytes } => {
                write!(f, "cannot read {bytes} bytes at offset {offset:#x}")
            }
        }
    }
}

impl Error for MzError {}

/// An immutable, validated view. Mutating a copied Header cannot invalidate it.
#[derive(Debug)]
pub struct MzFile<'a> {
    header: Header,
    data: &'a [u8],
    image: &'a [u8],
    declared_file_bytes: usize,
    header_bytes: usize,
    entry_image_offset: usize,
    entry_file_offset: usize,
    relocations: Vec<Relocation>,
}

/// An owned image-relative allocation; no PSP or DOS process state is included.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LoadedMz {
    pub memory: Vec<u8>,
    pub load_segment: u16,
    pub cs: u16,
    pub ip: u16,
    pub ss: u16,
    pub sp: u16,
    pub relocation_count: usize,
}

impl<'a> MzFile<'a> {
    /// Validate the container, entry point and every relocation word target.
    /// Load-address, allocation and initial-stack constraints are checked by load.
    pub fn parse(data: &'a [u8]) -> Result<Self, MzError> {
        if data.len() < FIXED_HEADER_BYTES {
            return Err(MzError::TruncatedHeader {
                actual_bytes: data.len(),
            });
        }
        let header = Header {
            magic: word(data, 0)?,
            last_page_bytes: word(data, 2)?,
            pages: word(data, 4)?,
            relocations: word(data, 6)?,
            header_paragraphs: word(data, 8)?,
            min_extra_paragraphs: word(data, 10)?,
            max_extra_paragraphs: word(data, 12)?,
            ss: word(data, 14)?,
            sp: word(data, 16)?,
            checksum: word(data, 18)?,
            ip: word(data, 20)?,
            cs: word(data, 22)?,
            relocation_table_offset: word(data, 24)?,
            overlay_number: word(data, 26)?,
        };
        if !matches!(header.magic, 0x5a4d | 0x4d5a) {
            return Err(MzError::InvalidMagic {
                magic: header.magic,
            });
        }
        if header.pages == 0 {
            return Err(MzError::InvalidPageCount);
        }
        if usize::from(header.last_page_bytes) >= PAGE_BYTES {
            return Err(MzError::InvalidLastPageBytes {
                value: header.last_page_bytes,
            });
        }
        let preceding_pages = usize::from(header.pages)
            .checked_sub(1)
            .ok_or(MzError::InvalidPageCount)?;
        let last_page = if header.last_page_bytes == 0 {
            PAGE_BYTES
        } else {
            usize::from(header.last_page_bytes)
        };
        let declared_file_bytes = add(
            mul(preceding_pages, PAGE_BYTES, "MZ pages")?,
            last_page,
            "declared file size",
        )?;
        let header_bytes = mul(
            usize::from(header.header_paragraphs),
            PARAGRAPH_BYTES,
            "header size",
        )?;
        if header_bytes < FIXED_HEADER_BYTES {
            return Err(MzError::HeaderTooSmall { header_bytes });
        }
        if header_bytes > declared_file_bytes {
            return Err(MzError::HeaderBeyondDeclared {
                header_bytes,
                declared_bytes: declared_file_bytes,
            });
        }
        if declared_file_bytes > data.len() {
            return Err(MzError::TruncatedFile {
                declared_bytes: declared_file_bytes,
                actual_bytes: data.len(),
            });
        }
        if header.min_extra_paragraphs > header.max_extra_paragraphs {
            return Err(MzError::InvalidExtraParagraphs {
                minimum: header.min_extra_paragraphs,
                maximum: header.max_extra_paragraphs,
            });
        }
        if header.overlay_number != 0 {
            return Err(MzError::UnsupportedOverlay {
                number: header.overlay_number,
            });
        }
        if let Some((kind, offset)) = secondary_header(data, &header, header_bytes)? {
            return Err(MzError::UnsupportedSecondaryHeader { kind, offset });
        }
        let image = data
            .get(header_bytes..declared_file_bytes)
            .ok_or(MzError::TruncatedField {
                offset: header_bytes,
                bytes: declared_file_bytes,
            })?;
        let entry_image_offset = linear(header.cs, usize::from(header.ip))? % REAL_MODE_BYTES;
        if entry_image_offset >= image.len() {
            return Err(MzError::EntryOutsideImage {
                offset: entry_image_offset,
                image_bytes: image.len(),
            });
        }
        let entry_file_offset = add(header_bytes, entry_image_offset, "entry file offset")?;
        let count = usize::from(header.relocations);
        let table_offset = usize::from(header.relocation_table_offset);
        let table_bytes = mul(count, 4, "relocation table length")?;
        let table_end = add(table_offset, table_bytes, "relocation table end")?;
        // With zero records there is no table: historical linkers leave arbitrary offsets.
        if count != 0 {
            if table_offset < FIXED_HEADER_BYTES {
                return Err(MzError::RelocationTableBeforeFixedHeader {
                    offset: table_offset,
                });
            }
            if table_end > header_bytes {
                return Err(MzError::RelocationTableBeyondHeader {
                    end: table_end,
                    header_bytes,
                });
            }
        }
        let mut relocations = Vec::new();
        relocations
            .try_reserve_exact(count)
            .map_err(|_| MzError::AllocationFailed { bytes: table_bytes })?;
        for index in 0..count {
            let at = add(
                table_offset,
                mul(index, 4, "relocation index")?,
                "relocation record offset",
            )?;
            let relocation = Relocation {
                offset: word(data, at)?,
                segment: word(data, add(at, 2, "relocation segment offset")?)?,
            };
            let offset = relocation.image_offset();
            let end = add(offset, 2, "relocation target end")?;
            if end > image.len() {
                return Err(MzError::RelocationTargetOutsideImage {
                    index,
                    offset,
                    image_bytes: image.len(),
                });
            }
            relocations.push(relocation);
        }
        Ok(Self {
            header,
            data,
            image,
            declared_file_bytes,
            header_bytes,
            entry_image_offset,
            entry_file_offset,
            relocations,
        })
    }

    pub fn header(&self) -> &Header {
        &self.header
    }
    pub fn image(&self) -> &'a [u8] {
        self.image
    }
    pub fn declared_file_bytes(&self) -> usize {
        self.declared_file_bytes
    }
    pub fn actual_file_bytes(&self) -> usize {
        self.data.len()
    }
    pub fn header_bytes(&self) -> usize {
        self.header_bytes
    }
    pub fn entry_image_offset(&self) -> usize {
        self.entry_image_offset
    }
    pub fn entry_file_offset(&self) -> usize {
        self.entry_file_offset
    }
    pub fn relocations(&self) -> &[Relocation] {
        &self.relocations
    }

    /// Trailing overlays/debug information, retained but never copied into memory.
    pub fn trailing_bytes(&self) -> &'a [u8] {
        // The immutable constructor proved this bound.
        self.data
            .get(self.declared_file_bytes..)
            .unwrap_or_default()
    }

    pub fn load(&self, load_segment: u16) -> Result<LoadedMz, MzError> {
        let rounded_paragraphs =
            add(self.image.len(), PARAGRAPH_BYTES - 1, "paragraph rounding")? / PARAGRAPH_BYTES;
        let paragraphs = add(
            rounded_paragraphs,
            usize::from(self.header.min_extra_paragraphs),
            "allocation paragraphs",
        )?;
        let memory_bytes = mul(paragraphs, PARAGRAPH_BYTES, "allocation size")?;
        let load_base = linear(load_segment, 0)?;
        let allocation_end = add(load_base, memory_bytes, "allocation end")?;
        if allocation_end > REAL_MODE_BYTES {
            return Err(MzError::MemoryOutsideRealMode {
                load_base,
                memory_bytes,
            });
        }
        let cs = self.header.cs.wrapping_add(load_segment);
        let expected_entry = add(load_base, self.entry_image_offset, "loaded entry")?;
        let actual_entry = linear(cs, usize::from(self.header.ip))?;
        if actual_entry != expected_entry || actual_entry >= REAL_MODE_BYTES {
            return Err(MzError::AddressWrap {
                register: "CS:IP",
                expected: expected_entry,
                actual: actual_entry,
            });
        }
        let stack_displacement = if self.header.sp == 0 {
            1 << 16
        } else {
            usize::from(self.header.sp)
        };
        if stack_displacement < 2 {
            return Err(MzError::StackPointerTooSmall { sp: self.header.sp });
        }
        let stack_top = linear(self.header.ss, stack_displacement)?;
        if stack_top > memory_bytes {
            return Err(MzError::StackOutsideMemory {
                offset: stack_top,
                memory_bytes,
            });
        }
        let ss = self.header.ss.wrapping_add(load_segment);
        let expected_stack = add(load_base, stack_top, "loaded logical stack top")?;
        let actual_stack = linear(ss, stack_displacement)?;
        if actual_stack != expected_stack || actual_stack > REAL_MODE_BYTES {
            return Err(MzError::AddressWrap {
                register: "SS:SP",
                expected: expected_stack,
                actual: actual_stack,
            });
        }
        let mut memory = Vec::new();
        memory
            .try_reserve_exact(memory_bytes)
            .map_err(|_| MzError::AllocationFailed {
                bytes: memory_bytes,
            })?;
        memory.resize(memory_bytes, 0);
        memory
            .get_mut(..self.image.len())
            .ok_or(MzError::TruncatedField {
                offset: 0,
                bytes: self.image.len(),
            })?
            .copy_from_slice(self.image);
        // Preserve table order, including duplicates and overlapping word targets.
        for relocation in &self.relocations {
            let offset = relocation.image_offset();
            let value = word(&memory, offset)?.wrapping_add(load_segment);
            let end = add(offset, 2, "loaded relocation end")?;
            memory
                .get_mut(offset..end)
                .ok_or(MzError::TruncatedField { offset, bytes: 2 })?
                .copy_from_slice(&value.to_le_bytes());
        }
        Ok(LoadedMz {
            memory,
            load_segment,
            cs,
            ip: self.header.ip,
            ss,
            sp: self.header.sp,
            relocation_count: self.relocations.len(),
        })
    }
}

fn add(left: usize, right: usize, context: &'static str) -> Result<usize, MzError> {
    left.checked_add(right)
        .ok_or(MzError::ArithmeticOverflow { context })
}

fn mul(left: usize, right: usize, context: &'static str) -> Result<usize, MzError> {
    left.checked_mul(right)
        .ok_or(MzError::ArithmeticOverflow { context })
}

fn linear(segment: u16, offset: usize) -> Result<usize, MzError> {
    add(
        mul(usize::from(segment), PARAGRAPH_BYTES, "segment base")?,
        offset,
        "segment address",
    )
}

fn bytes<const N: usize>(data: &[u8], offset: usize) -> Result<[u8; N], MzError> {
    let end = add(offset, N, "field end")?;
    data.get(offset..end)
        .and_then(|part| part.try_into().ok())
        .ok_or(MzError::TruncatedField { offset, bytes: N })
}

fn word(data: &[u8], offset: usize) -> Result<u16, MzError> {
    Ok(u16::from_le_bytes(bytes(data, offset)?))
}

fn secondary_header(
    data: &[u8],
    header: &Header,
    header_bytes: usize,
) -> Result<Option<(SecondaryHeaderKind, usize)>, MzError> {
    // Q65122's extended DOS-header convention distinguishes e_lfanew from an
    // old relocation entry or arbitrary padding at 0x3c. Modern PE stubs also
    // commonly set e_lfarlc=0 with no relocations; accept that convention too.
    // Do not infer a new header merely from a large old MZ header.
    if header_bytes < 64
        || !(header.relocation_table_offset == 0x40
            || (header.relocation_table_offset == 0 && header.relocations == 0))
    {
        return Ok(None);
    }
    let offset = usize::try_from(u32::from_le_bytes(bytes(data, 0x3c)?)).map_err(|_| {
        MzError::ArithmeticOverflow {
            context: "secondary header offset",
        }
    })?;
    if offset < 64 {
        return Ok(None);
    }
    let Some(signature) = data.get(offset..) else {
        return Ok(None);
    };
    let kind = if signature.starts_with(b"NE") {
        Some(SecondaryHeaderKind::Ne)
    } else if signature.starts_with(b"LE") {
        Some(SecondaryHeaderKind::Le)
    } else if signature.starts_with(b"LX") {
        Some(SecondaryHeaderKind::Lx)
    } else if signature.starts_with(b"PE\0\0") {
        Some(SecondaryHeaderKind::Pe)
    } else {
        None
    };
    Ok(kind.map(|kind| (kind, offset)))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn put_word(data: &mut [u8], at: usize, value: u16) {
        data[at..at + 2].copy_from_slice(&value.to_le_bytes());
    }

    fn fixture(image: &[u8], records: &[Relocation]) -> Vec<u8> {
        let header_bytes = (FIXED_HEADER_BYTES + records.len() * 4).div_ceil(16) * 16;
        let declared = header_bytes + image.len();
        let mut data = vec![0; declared];
        data[..2].copy_from_slice(b"MZ");
        put_word(&mut data, 2, (declared % 512) as u16);
        put_word(&mut data, 4, declared.div_ceil(512) as u16);
        put_word(&mut data, 6, records.len() as u16);
        put_word(&mut data, 8, (header_bytes / 16) as u16);
        put_word(&mut data, 12, u16::MAX);
        put_word(&mut data, 16, 16);
        put_word(&mut data, 24, FIXED_HEADER_BYTES as u16);
        for (index, record) in records.iter().enumerate() {
            put_word(&mut data, FIXED_HEADER_BYTES + index * 4, record.offset);
            put_word(
                &mut data,
                FIXED_HEADER_BYTES + index * 4 + 2,
                record.segment,
            );
        }
        data[header_bytes..].copy_from_slice(image);
        data
    }

    fn extended_fixture() -> Vec<u8> {
        let mut data = fixture(&[0; 160], &[]);
        put_word(&mut data, 8, 4);
        put_word(&mut data, 24, 0x40);
        data[0x3c..0x40].copy_from_slice(&128u32.to_le_bytes());
        data
    }

    #[test]
    fn parses_all_header_fields_and_exact_image() {
        let mut data = fixture(&[1, 2, 3], &[]);
        put_word(&mut data, 18, 0x1234);
        let mz = MzFile::parse(&data).unwrap();
        assert_eq!(
            mz.header(),
            &Header {
                magic: 0x5a4d,
                last_page_bytes: 35,
                pages: 1,
                relocations: 0,
                header_paragraphs: 2,
                min_extra_paragraphs: 0,
                max_extra_paragraphs: 65535,
                ss: 0,
                sp: 16,
                checksum: 0x1234,
                ip: 0,
                cs: 0,
                relocation_table_offset: 28,
                overlay_number: 0
            }
        );
        assert_eq!(mz.image(), &[1, 2, 3]);
        assert_eq!(mz.header_bytes(), 32);
        assert_eq!(mz.declared_file_bytes(), 35);
        assert_eq!(mz.actual_file_bytes(), 35);
        assert_eq!(mz.entry_file_offset(), 32);
        assert_eq!(mz.entry_image_offset(), 0);
        assert!(mz.relocations().is_empty());
        let mut detached = *mz.header();
        detached.cs = u16::MAX;
        assert_ne!(detached.cs, mz.header().cs);
        assert!(mz.load(0x1000).is_ok());
    }

    #[test]
    fn supports_zm_and_zero_last_page_count_means_full_page() {
        let mut data = fixture(&[0; 480], &[]);
        data[..2].copy_from_slice(b"ZM");
        let mz = MzFile::parse(&data).unwrap();
        assert_eq!(mz.header().last_page_bytes, 0);
        assert_eq!(mz.declared_file_bytes(), 512);
        assert_eq!(mz.image().len(), 480);
    }

    #[test]
    fn allocation_is_image_padding_and_minimum_extra_only() {
        let mut data = fixture(&[0xa5; 17], &[]);
        put_word(&mut data, 10, 2);
        put_word(&mut data, 14, 1);
        put_word(&mut data, 16, 48);
        let mz = MzFile::parse(&data).unwrap();
        let loaded = mz.load(0x1234).unwrap();
        assert_eq!(loaded.memory.len(), 64);
        assert_eq!(&loaded.memory[..17], &[0xa5; 17]);
        assert!(loaded.memory[17..].iter().all(|&byte| byte == 0));
        assert_eq!(
            (
                loaded.load_segment,
                loaded.cs,
                loaded.ip,
                loaded.ss,
                loaded.sp
            ),
            (0x1234, 0x1234, 0, 0x1235, 48)
        );
        assert_eq!(loaded.relocation_count, 0);
    }

    #[test]
    fn trailing_file_bytes_are_retained_but_never_loaded() {
        let mut data = fixture(&[1, 2, 3], &[]);
        data.extend_from_slice(&[0xcd; 700]);
        let mz = MzFile::parse(&data).unwrap();
        assert_eq!(mz.declared_file_bytes(), 35);
        assert_eq!(mz.actual_file_bytes(), 735);
        assert_eq!(mz.trailing_bytes(), &[0xcd; 700]);
        assert_eq!(
            mz.load(0).unwrap().memory,
            [1, 2, 3, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0]
        );
    }

    #[test]
    fn all_truncated_prefixes_are_rejected_without_panics() {
        let data = fixture(
            &[0; 64],
            &[Relocation {
                offset: 0,
                segment: 0,
            }],
        );
        for end in 0..data.len() {
            assert!(MzFile::parse(&data[..end]).is_err(), "prefix {end}");
        }
    }

    #[test]
    fn invalid_size_and_magic_fields_are_rejected() {
        for (field, value) in [
            (0, 0),
            (4, 0),
            (2, 512),
            (2, u16::MAX),
            (8, 0),
            (8, 1),
            (8, u16::MAX),
            (4, u16::MAX),
        ] {
            let mut data = fixture(&[0; 16], &[]);
            put_word(&mut data, field, value);
            assert!(
                MzFile::parse(&data).is_err(),
                "field {field}, value {value}"
            );
        }
        let data = fixture(&[], &[]);
        assert!(matches!(
            MzFile::parse(&data),
            Err(MzError::EntryOutsideImage { .. })
        ));
    }

    #[test]
    fn overlays_and_inconsistent_extra_allocation_are_rejected() {
        let mut data = fixture(&[0; 16], &[]);
        put_word(&mut data, 26, 1);
        assert!(matches!(
            MzFile::parse(&data),
            Err(MzError::UnsupportedOverlay { number: 1 })
        ));
        put_word(&mut data, 26, 0);
        put_word(&mut data, 10, 2);
        put_word(&mut data, 12, 1);
        assert!(matches!(
            MzFile::parse(&data),
            Err(MzError::InvalidExtraParagraphs {
                minimum: 2,
                maximum: 1
            })
        ));
    }

    #[test]
    fn relocation_tables_cannot_overlap_fixed_header_or_exceed_header() {
        for offset in [0, 2, 26, 27, 29, u16::MAX] {
            let mut data = fixture(
                &[0; 16],
                &[Relocation {
                    offset: 0,
                    segment: 0,
                }],
            );
            put_word(&mut data, 24, offset);
            assert!(matches!(
                MzFile::parse(&data),
                Err(MzError::RelocationTableBeforeFixedHeader { .. }
                    | MzError::RelocationTableBeyondHeader { .. })
            ));
        }
        let mut data = fixture(&[0; 16], &[]);
        put_word(&mut data, 6, u16::MAX);
        assert!(matches!(
            MzFile::parse(&data),
            Err(MzError::RelocationTableBeyondHeader { .. })
        ));
    }

    #[test]
    fn empty_relocation_table_does_not_require_a_valid_offset() {
        let mut data = fixture(&[0; 16], &[]);
        for offset in [0, 27, u16::MAX] {
            put_word(&mut data, 24, offset);
            assert!(MzFile::parse(&data).is_ok());
        }
    }

    #[test]
    fn relocation_words_must_be_fully_inside_unpadded_image() {
        for record in [
            Relocation {
                offset: 2,
                segment: 0,
            },
            Relocation {
                offset: 3,
                segment: 0,
            },
            Relocation {
                offset: 0,
                segment: 1,
            },
            Relocation {
                offset: 16,
                segment: u16::MAX,
            },
            Relocation {
                offset: u16::MAX,
                segment: u16::MAX,
            },
        ] {
            let mut data = fixture(&[0; 3], &[record]);
            put_word(&mut data, 10, 10);
            assert!(matches!(
                MzFile::parse(&data),
                Err(MzError::RelocationTargetOutsideImage { index: 0, .. })
            ));
        }
        let data = fixture(
            &[0; 3],
            &[Relocation {
                offset: 1,
                segment: 0,
            }],
        );
        assert!(MzFile::parse(&data).is_ok());
        assert_eq!(
            Relocation {
                offset: u16::MAX,
                segment: u16::MAX
            }
            .image_offset(),
            0x10ffef
        );
    }

    #[test]
    fn every_relocation_is_applied_with_word_wrapping_without_mutating_input() {
        let mut image = vec![0; 40];
        put_word(&mut image, 0, 0xffff);
        put_word(&mut image, 18, 0x1234);
        put_word(&mut image, 38, 0xff00);
        let records = [
            Relocation {
                offset: 0,
                segment: 0,
            },
            Relocation {
                offset: 2,
                segment: 1,
            },
            Relocation {
                offset: 6,
                segment: 2,
            },
        ];
        let data = fixture(&image, &records);
        let original = data.clone();
        let mz = MzFile::parse(&data).unwrap();
        assert_eq!(mz.relocations(), &records);
        let loaded = mz.load(0x100).unwrap();
        assert_eq!(loaded.relocation_count, 3);
        assert_eq!(word(&loaded.memory, 0).unwrap(), 0x00ff);
        assert_eq!(word(&loaded.memory, 18).unwrap(), 0x1334);
        assert_eq!(word(&loaded.memory, 38).unwrap(), 0);
        assert_eq!(data, original);
        assert_eq!(mz.load(0).unwrap().memory[..40], image);
        assert_eq!(mz.load(0x100).unwrap(), loaded);
    }

    #[test]
    fn duplicate_and_overlapping_relocations_preserve_table_order() {
        let image = [0xfe, 0xfe, 0xff, 0];
        let a = Relocation {
            offset: 0,
            segment: 0,
        };
        let b = Relocation {
            offset: 1,
            segment: 0,
        };
        let data = fixture(&image, &[a, b, a]);
        let loaded = MzFile::parse(&data).unwrap().load(0x101).unwrap();
        assert_eq!(&loaded.memory[..4], &[0, 2, 1, 0]);
        assert_eq!(loaded.relocation_count, 3);
        let reversed = fixture(&image, &[a, a, b]);
        assert_ne!(
            MzFile::parse(&reversed)
                .unwrap()
                .load(0x101)
                .unwrap()
                .memory,
            loaded.memory
        );
    }

    #[test]
    fn entry_must_be_inside_declared_image_not_padding_or_extra_or_tail() {
        let mut data = fixture(&[0; 3], &[]);
        put_word(&mut data, 10, 4);
        data.extend_from_slice(&[0; 16]);
        for ip in [3, 15, 16, 64, u16::MAX] {
            put_word(&mut data, 20, ip);
            assert!(matches!(
                MzFile::parse(&data),
                Err(MzError::EntryOutsideImage { .. })
            ));
        }
        put_word(&mut data, 20, 2);
        assert_eq!(MzFile::parse(&data).unwrap().entry_file_offset(), 34);
    }

    #[test]
    fn packed_negative_cs_is_relative_but_requires_a_consistent_physical_entry() {
        let mut data = fixture(&[0; 32], &[]);
        put_word(&mut data, 22, 0xfff0);
        put_word(&mut data, 20, 0x100);
        let mz = MzFile::parse(&data).unwrap();
        assert_eq!(mz.entry_image_offset(), 0);
        assert_eq!(mz.entry_file_offset(), 32);
        for segment in [0, 1, 0xf] {
            assert!(matches!(
                mz.load(segment),
                Err(MzError::AddressWrap {
                    register: "CS:IP",
                    ..
                })
            ));
        }
        for segment in [0x10, 0x1000, 0xfffe] {
            let loaded = mz.load(segment).unwrap();
            assert_eq!(
                linear(loaded.cs, usize::from(loaded.ip)).unwrap(),
                linear(segment, 0).unwrap()
            );
        }
    }

    #[test]
    fn modulo_entry_does_not_rescue_an_out_of_image_entry() {
        let mut data = fixture(&[0; 16], &[]);
        put_word(&mut data, 22, 0xfff0);
        put_word(&mut data, 20, 0xff);
        assert!(matches!(
            MzFile::parse(&data),
            Err(MzError::EntryOutsideImage {
                offset: 0xfffff,
                ..
            })
        ));
        put_word(&mut data, 20, 0x110);
        assert!(matches!(
            MzFile::parse(&data),
            Err(MzError::EntryOutsideImage { offset: 16, .. })
        ));
    }

    #[test]
    fn allocation_checks_include_rounding_minimum_extra_and_load_base() {
        let mut data = fixture(&[0; 1], &[]);
        assert_eq!(
            MzFile::parse(&data)
                .unwrap()
                .load(0xffff)
                .unwrap()
                .memory
                .len(),
            16
        );
        put_word(&mut data, 10, 1);
        assert!(matches!(
            MzFile::parse(&data).unwrap().load(0xffff),
            Err(MzError::MemoryOutsideRealMode { .. })
        ));
        put_word(&mut data, 10, u16::MAX);
        assert_eq!(
            MzFile::parse(&data).unwrap().load(0).unwrap().memory.len(),
            REAL_MODE_BYTES
        );
        assert!(matches!(
            MzFile::parse(&data).unwrap().load(1),
            Err(MzError::MemoryOutsideRealMode { .. })
        ));
        let mut data = fixture(&[0; 17], &[]);
        put_word(&mut data, 10, u16::MAX);
        assert!(matches!(
            MzFile::parse(&data).unwrap().load(0),
            Err(MzError::MemoryOutsideRealMode { .. })
        ));
    }

    #[test]
    fn oversized_image_is_inspectable_but_not_loadable() {
        let data = fixture(&vec![0; REAL_MODE_BYTES + 1], &[]);
        let mz = MzFile::parse(&data).unwrap();
        assert!(matches!(
            mz.load(0),
            Err(MzError::MemoryOutsideRealMode { .. })
        ));
    }

    #[test]
    fn stack_one_past_end_is_allowed_but_out_of_bounds_and_wrap_alias_are_not() {
        let mut data = fixture(&[0; 16], &[]);
        assert!(MzFile::parse(&data).unwrap().load(0).is_ok());
        assert!(MzFile::parse(&data).unwrap().load(0xffff).is_ok());
        put_word(&mut data, 16, 17);
        assert!(matches!(
            MzFile::parse(&data).unwrap().load(0),
            Err(MzError::StackOutsideMemory { .. })
        ));
        put_word(&mut data, 16, 1);
        assert!(matches!(
            MzFile::parse(&data).unwrap().load(0),
            Err(MzError::StackPointerTooSmall { sp: 1 })
        ));
        put_word(&mut data, 16, 16);
        put_word(&mut data, 14, u16::MAX);
        assert!(matches!(
            MzFile::parse(&data).unwrap().load(1),
            Err(MzError::StackOutsideMemory { .. })
        ));
    }

    #[test]
    fn zero_sp_requires_a_full_stack_segment_but_remains_zero_in_registers() {
        let mut data = fixture(&[0; 16], &[]);
        put_word(&mut data, 16, 0);
        assert!(matches!(
            MzFile::parse(&data).unwrap().load(0),
            Err(MzError::StackOutsideMemory { offset: 65536, .. })
        ));
        put_word(&mut data, 10, 4095);
        let mz = MzFile::parse(&data).unwrap();
        let loaded = mz.load(0xf000).unwrap();
        assert_eq!(loaded.sp, 0);
        assert_eq!(loaded.memory.len(), 65536);
        assert_eq!(loaded.ss, 0xf000);
        assert!(matches!(
            mz.load(0xf001),
            Err(MzError::MemoryOutsideRealMode { .. })
        ));
    }

    #[test]
    fn recognized_secondary_headers_are_rejected_instead_of_loading_dos_stub() {
        for (signature, kind) in [
            (b"NE\0\0", SecondaryHeaderKind::Ne),
            (b"LE\0\0", SecondaryHeaderKind::Le),
            (b"LX\0\0", SecondaryHeaderKind::Lx),
            (b"PE\0\0", SecondaryHeaderKind::Pe),
        ] {
            for relocation_offset in [0, 0x40] {
                let mut data = extended_fixture();
                put_word(&mut data, 24, relocation_offset);
                data[128..132].copy_from_slice(signature);
                assert!(
                    matches!(MzFile::parse(&data), Err(MzError::UnsupportedSecondaryHeader { kind: actual, offset: 128 }) if actual == kind)
                );
            }
        }
    }

    #[test]
    fn secondary_headers_in_trailing_bytes_are_also_detected() {
        let mut data = extended_fixture();
        put_word(&mut data, 2, 96);
        data[128..132].copy_from_slice(b"PE\0\0");
        assert!(matches!(
            MzFile::parse(&data),
            Err(MzError::UnsupportedSecondaryHeader {
                kind: SecondaryHeaderKind::Pe,
                offset: 128
            })
        ));
    }

    #[test]
    fn old_headers_and_invalid_secondary_pointers_do_not_produce_false_positives() {
        let mut data = extended_fixture();
        data[128..132].copy_from_slice(b"PE\0\0");
        for relocation_offset in [28, 30, 0x52] {
            put_word(&mut data, 24, relocation_offset);
            assert!(
                MzFile::parse(&data).is_ok(),
                "old relocation offset {relocation_offset}"
            );
        }
        put_word(&mut data, 24, 0x40);
        for offset in [0u32, 1, 28, 60, 63, 191, 192, 193, u32::MAX] {
            data[0x3c..0x40].copy_from_slice(&offset.to_le_bytes());
            assert!(MzFile::parse(&data).is_ok(), "e_lfanew {offset}");
        }
        data[0x3c..0x40].copy_from_slice(&128u32.to_le_bytes());
        put_word(&mut data, 8, 2);
        assert!(MzFile::parse(&data).is_ok());
    }

    #[test]
    fn pe_requires_exact_four_byte_signature_not_just_pe_prefix() {
        for signature in [b"PEab", b"PE\0x", b"PE x"] {
            let mut data = extended_fixture();
            data[128..132].copy_from_slice(signature);
            assert!(MzFile::parse(&data).is_ok());
        }
        let mut data = extended_fixture();
        data[190..192].copy_from_slice(b"PE");
        data[0x3c..0x40].copy_from_slice(&190u32.to_le_bytes());
        assert!(MzFile::parse(&data).is_ok());
        data[190..192].copy_from_slice(b"NE");
        assert!(matches!(
            MzFile::parse(&data),
            Err(MzError::UnsupportedSecondaryHeader {
                kind: SecondaryHeaderKind::Ne,
                offset: 190
            })
        ));
    }

    #[test]
    fn helpers_fail_on_overflow_and_out_of_bounds_not_panic() {
        assert!(matches!(
            add(usize::MAX, 1, "test"),
            Err(MzError::ArithmeticOverflow { .. })
        ));
        assert!(matches!(
            mul(usize::MAX, 2, "test"),
            Err(MzError::ArithmeticOverflow { .. })
        ));
        assert!(matches!(
            word(&[], usize::MAX),
            Err(MzError::ArithmeticOverflow { .. })
        ));
        assert!(matches!(word(&[1], 0), Err(MzError::TruncatedField { .. })));
        assert!(!MzError::InvalidPageCount.to_string().is_empty());
    }

    #[test]
    fn deterministic_adversarial_inputs_do_not_panic() {
        let base = fixture(
            &[0x55; 128],
            &[Relocation {
                offset: 2,
                segment: 0,
            }],
        );
        let mut state = 0x1234_5678u32;
        for iteration in 0..4000 {
            let mut data = base.clone();
            for _ in 0..(iteration % 8 + 1) {
                state = state.wrapping_mul(1664525).wrapping_add(1013904223);
                let at = (state as usize) % data.len();
                data[at] = (state >> 24) as u8;
            }
            if let Ok(mz) = MzFile::parse(&data) {
                let segment = state as u16;
                if let Ok(loaded) = mz.load(segment) {
                    assert!(linear(segment, loaded.memory.len()).unwrap() <= REAL_MODE_BYTES);
                    assert_eq!(
                        linear(loaded.cs, usize::from(loaded.ip)).unwrap(),
                        linear(segment, mz.entry_image_offset()).unwrap()
                    );
                }
            }
        }
    }

    /// Explicit opt-in: no proprietary bytes embedded in source or default tests.
    #[test]
    #[ignore = "set PAL_MZ_TEST_INPUT to local PAL.raw.exe; static read only"]
    fn local_pal_raw_static_acceptance() {
        let path =
            std::env::var_os("PAL_MZ_TEST_INPUT").expect("PAL_MZ_TEST_INPUT must name PAL.raw.exe");
        let data = std::fs::read(path).unwrap();
        let original = data.clone();
        let mz = MzFile::parse(&data).unwrap();
        let header = mz.header();
        assert_eq!(mz.actual_file_bytes(), 210128);
        assert_eq!(mz.declared_file_bytes(), 210128);
        assert_eq!(header.header_paragraphs, 596);
        assert_eq!(header.relocations, 2376);
        assert_eq!(
            (header.cs, header.ip, header.ss, header.sp),
            (9140, 1068, 12537, 3072)
        );
        assert_eq!(
            (header.min_extra_paragraphs, header.max_extra_paragraphs),
            (327, 65535)
        );
        assert_eq!(mz.header_bytes(), 9536);
        assert_eq!(mz.entry_file_offset(), 0x264ac);
        assert_eq!(mz.entry_image_offset(), 147308);
        assert_eq!(mz.image().len(), 200592);
        assert!(mz.trailing_bytes().is_empty());
        let mut expected = mz.image().to_vec();
        expected.resize(205824, 0);
        let load_segment = 0x1000u16;
        for record in mz.relocations() {
            let at = record.image_offset();
            let value = u16::from_le_bytes([expected[at], expected[at + 1]]);
            expected[at..at + 2].copy_from_slice(&value.wrapping_add(load_segment).to_le_bytes());
        }
        let loaded = mz.load(load_segment).unwrap();
        assert_eq!(loaded.memory, expected);
        assert_eq!(loaded.relocation_count, 2376);
        assert_eq!(loaded.cs, header.cs.wrapping_add(load_segment));
        assert_eq!(loaded.ss, header.ss.wrapping_add(load_segment));
        assert_eq!((loaded.ip, loaded.sp), (1068, 3072));
        assert_eq!(data, original);
        println!(
            "PAL.raw.exe: actual={}, declared={}, header={}, image={}, allocation={}, relocations={}, entry_file={:#x}, CS:IP={:04x}:{:04x}, SS:SP={:04x}:{:04x}; static load only",
            mz.actual_file_bytes(),
            mz.declared_file_bytes(),
            mz.header_bytes(),
            mz.image().len(),
            loaded.memory.len(),
            loaded.relocation_count,
            mz.entry_file_offset(),
            loaded.cs,
            loaded.ip,
            loaded.ss,
            loaded.sp
        );
    }
}
