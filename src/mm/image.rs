//! The kernel's own PE image, as the firmware loaded it: where it is and
//! what each section may be used for, so that the page tables can map code
//! read-only and executable, and everything else non-executable (W^X).
//!
//! The image starts at `__ImageBase`, which the linker (lld-link) defines.
//! Sections must start on page boundaries, or one page could need two
//! kinds of access; `sections` checks that.

use crate::mm::PAGE_SIZE;
use alloc::vec::Vec;

const IMAGE_SCN_MEM_EXECUTE: u32 = 0x2000_0000;
const IMAGE_SCN_MEM_WRITE: u32 = 0x8000_0000;
/// PE32+ optional header magic.
const PE32_PLUS: u16 = 0x20b;
/// Bytes of a section table entry.
const SECTION_SIZE: usize = 40;

unsafe extern "C" {
    /// First byte of the loaded image: its DOS header.
    static __ImageBase: u8;
}

/// What a range of the image may be used for.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Access {
    /// Read and execute.
    Code,
    /// Read only: headers, constants, relocations.
    ReadOnly,
    /// Read and write.
    Data,
}

/// One range of the loaded image, in whole pages.
#[derive(Clone, Copy, Debug)]
pub struct Section {
    pub start: u64,
    pub len: u64,
    pub access: Access,
}

/// The whole image as a byte slice, `SizeOfImage` long, after checking the
/// headers that say how long it is.
fn image() -> Result<&'static [u8], &'static str> {
    let base = &raw const __ImageBase;
    // SAFETY: the DOS header is 64 bytes, always present at the image base.
    let dos = unsafe { core::slice::from_raw_parts(base, 64) };
    if &dos[..2] != b"MZ" {
        return Err("no MZ header at __ImageBase");
    }
    let pe = u32_at(dos, 0x3c) as usize;
    if pe > PAGE_SIZE {
        return Err("PE header too far in");
    }
    // SAFETY: the PE signature, COFF header and the optional header fields we read lie
    // within a page and a bit of the base (checked above), all part of the headers the
    // loader copied in, which fill at least the first page.
    let head = unsafe { core::slice::from_raw_parts(base, pe + 24 + 64) };
    if &head[pe..pe + 4] != b"PE\0\0" {
        return Err("no PE signature");
    }
    if u16_at(head, pe + 24) != PE32_PLUS {
        return Err("not a PE32+ image");
    }
    let size = u32_at(head, pe + 24 + 56) as usize;
    // SAFETY: `SizeOfImage` bytes from the base are the image the loader mapped.
    Ok(unsafe { core::slice::from_raw_parts(base, size) })
}

fn u16_at(b: &[u8], off: usize) -> u16 {
    u16::from_le_bytes([b[off], b[off + 1]])
}

fn u32_at(b: &[u8], off: usize) -> u32 {
    u32::from_le_bytes(b[off..off + 4].try_into().unwrap())
}

/// The headers and every section of the loaded image, with their access.
pub fn sections() -> Result<Vec<Section>, &'static str> {
    let img = image()?;
    let base = img.as_ptr() as u64;
    let page = PAGE_SIZE as u64;
    let pe = u32_at(img, 0x3c) as usize;
    let count = u16_at(img, pe + 6) as usize;
    let opt_size = u16_at(img, pe + 20) as usize;
    let opt = pe + 24;
    if (u32_at(img, opt + 32) as usize) < PAGE_SIZE {
        return Err("sections are not page aligned");
    }
    let headers = u32_at(img, opt + 60) as u64;

    let mut out = Vec::from([Section {
        start: base,
        len: headers.next_multiple_of(page),
        access: Access::ReadOnly,
    }]);
    let table = opt + opt_size;
    for i in 0..count {
        let s = img
            .get(table + i * SECTION_SIZE..table + (i + 1) * SECTION_SIZE)
            .ok_or("section table outside the image")?;
        let (len, rva) = (u32_at(s, 8) as u64, u32_at(s, 12) as u64);
        let flags = u32_at(s, 36);
        if rva + len > img.len() as u64 {
            return Err("section outside the image");
        }
        let access = if flags & IMAGE_SCN_MEM_EXECUTE != 0 {
            if flags & IMAGE_SCN_MEM_WRITE != 0 {
                return Err("a section is both writable and executable");
            }
            Access::Code
        } else if flags & IMAGE_SCN_MEM_WRITE != 0 {
            Access::Data
        } else {
            Access::ReadOnly
        };
        out.push(Section { start: base + rva, len: len.next_multiple_of(page), access });
    }
    Ok(out)
}
