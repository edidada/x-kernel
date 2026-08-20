// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! ELF information parsed from the ELF file

use alloc::vec::Vec;
use core::{mem, ops::Range, ptr};

use xmas_elf::{
    header::Class,
    program::{ProgramHeader32, ProgramHeader64},
};

use crate::auxv::{AuxEntry, AuxType};

pub struct ELFHeadersBuilder<'a>(ELFHeaders<'a>);
impl<'a> ELFHeadersBuilder<'a> {
    pub fn new(input: &'a [u8]) -> Result<Self, &'static str> {
        Ok(Self(ELFHeaders {
            header: xmas_elf::header::parse_header(input)?,
            ph: Vec::new(),
        }))
    }

    /// File range covering the program header table.
    ///
    /// # Errors
    ///
    /// Returns an error when `e_phoff + e_phentsize * e_phnum` overflows.
    pub fn ph_range(&self) -> Result<Range<u64>, &'static str> {
        let start = self.0.header.pt2.ph_offset();
        let entry_size = u64::from(self.0.header.pt2.ph_entry_size());
        let count = u64::from(self.0.header.pt2.ph_count());
        let size = entry_size
            .checked_mul(count)
            .ok_or("program header table size overflow")?;
        let end = start
            .checked_add(size)
            .ok_or("program header table range overflow")?;
        Ok(start..end)
    }

    pub fn build(mut self, ph: &[u8]) -> Result<ELFHeaders<'a>, &'static str> {
        let count = self.0.header.pt2.ph_count() as usize;
        if count == 0 {
            return Ok(self.0);
        }

        let ph_entry_size = self.0.header.pt2.ph_entry_size() as usize;
        if ph_entry_size == 0 {
            return Err("invalid program header entry size");
        }

        let expected_size = match self.0.header.pt1.class() {
            Class::ThirtyTwo => mem::size_of::<ProgramHeader32>(),
            Class::SixtyFour => mem::size_of::<ProgramHeader64>(),
            Class::None | Class::Other(_) => return Err("unsupported ELF class"),
        };
        if ph_entry_size < expected_size {
            return Err("program header entry too small");
        }

        self.0.ph = ph
            .chunks_exact(ph_entry_size)
            .take(count)
            .map(|chunk| match self.0.header.pt1.class() {
                Class::ThirtyTwo => {
                    let ph = read_unaligned::<ProgramHeader32>(chunk)?;
                    Ok(ProgramHeader64 {
                        type_: ph.type_,
                        offset: ph.offset as _,
                        virtual_addr: ph.virtual_addr as _,
                        physical_addr: ph.physical_addr as _,
                        file_size: ph.file_size as _,
                        mem_size: ph.mem_size as _,
                        flags: ph.flags,
                        align: ph.align as _,
                    })
                }
                Class::SixtyFour => read_unaligned::<ProgramHeader64>(chunk),
                Class::None | Class::Other(_) => Err("unsupported ELF class"),
            })
            .collect::<Result<Vec<_>, _>>()?;
        if self.0.ph.len() != count {
            return Err("truncated program header table");
        }
        Ok(self.0)
    }
}

fn read_unaligned<T>(bytes: &[u8]) -> Result<T, &'static str> {
    let src = bytes
        .get(..mem::size_of::<T>())
        .ok_or("invalid program header size")?;
    // SAFETY: `src` contains `size_of::<T>()` readable bytes. `read_unaligned`
    // copies those bits out without requiring `src` to be aligned for `T`.
    let value = unsafe { ptr::read_unaligned(src.as_ptr().cast::<T>()) };
    Ok(value)
}

pub struct ELFHeaders<'a> {
    pub header: xmas_elf::header::Header<'a>,
    pub ph: Vec<ProgramHeader64>,
}

/// A wrapper for the ELF file data with some useful methods.
pub struct ELFParser<'a> {
    headers: &'a ELFHeaders<'a>,
    /// Base address of the ELF file loaded into the memory.
    base: usize,
}

impl<'a> ELFParser<'a> {
    /// Create a new `ELFInfo` instance.
    pub fn new(headers: &'a ELFHeaders<'a>, bias: usize) -> Result<Self, &'static str> {
        let base = if headers.header.pt2.type_().as_type() == xmas_elf::header::Type::SharedObject {
            bias
        } else {
            0
        };
        Ok(Self { headers, base })
    }

    /// The entry point of the ELF file, plus the load bias for `ET_DYN`.
    ///
    /// # Errors
    ///
    /// Returns an error when `e_entry + base` overflows `usize`.
    pub fn entry(&self) -> Result<usize, &'static str> {
        let entry_point = usize::try_from(self.headers.header.pt2.entry_point())
            .map_err(|_| "entry point overflow")?;
        entry_point
            .checked_add(self.base)
            .ok_or("entry point + base overflow")
    }

    /// The number of program headers in the ELF file.
    pub fn phnum(&self) -> usize {
        self.headers.header.pt2.ph_count() as usize
    }

    /// The size of the program header table entry in the ELF file.
    pub fn phent(&self) -> usize {
        self.headers.header.pt2.ph_entry_size() as usize
    }

    /// The offset of the program header table in the ELF file.
    ///
    /// # Errors
    ///
    /// Returns an error when no loaded segment covers the program header table,
    /// or when the resulting address overflows.
    pub fn phdr(&self) -> Result<usize, &'static str> {
        let ph_offset = self.headers.header.pt2.ph_offset();
        let header = self
            .headers
            .ph
            .iter()
            .find(|header| segment_contains_offset(header, ph_offset))
            .ok_or("can not find program header table address in elf")?;

        let file_off = usize::try_from(header.offset).map_err(|_| "phdr file offset overflow")?;
        let ph_off = usize::try_from(ph_offset).map_err(|_| "phdr table offset overflow")?;
        let vaddr = usize::try_from(header.virtual_addr).map_err(|_| "phdr vaddr overflow")?;
        let in_segment = ph_off
            .checked_sub(file_off)
            .ok_or("phdr offset precedes segment")?;
        in_segment
            .checked_add(vaddr)
            .and_then(|addr| addr.checked_add(self.base))
            .ok_or("phdr address overflow")
    }

    /// The base address of the ELF file loaded into the memory.
    pub fn base(&self) -> usize {
        self.base
    }

    pub fn headers(&self) -> &'a ELFHeaders<'a> {
        self.headers
    }

    /// Part of auxiliary vectors from the ELF file.
    ///
    /// # Arguments
    ///
    /// * `pagesz` - The page size of the system
    /// * `ldso_base` - The base address of the dynamic linker (if exists)
    ///
    /// # Errors
    ///
    /// Returns an error when the program header table address cannot be
    /// derived from the loaded segments, or when `e_entry + base` overflows.
    ///
    /// Details about auxiliary vectors are described in <https://articles.manugarg.com/aboutelfauxiliaryvectors.html>
    pub fn aux_vector(
        &self,
        pagesz: usize,
        ldso_base: Option<usize>,
    ) -> Result<impl Iterator<Item = AuxEntry>, &'static str> {
        let phdr = self.phdr()?;
        let entry = self.entry()?;
        Ok([
            (AuxType::PHDR, phdr),
            (AuxType::PHENT, self.phent()),
            (AuxType::PHNUM, self.phnum()),
            (AuxType::PAGESZ, pagesz),
            (AuxType::ENTRY, entry),
        ]
        .into_iter()
        .chain(ldso_base.into_iter().map(|base| (AuxType::BASE, base)))
        .map(|(at, val)| AuxEntry::new(at, val)))
    }
}

fn segment_contains_offset(header: &ProgramHeader64, ph_offset: u64) -> bool {
    let Some(end) = header.offset.checked_add(header.file_size) else {
        return false;
    };
    (header.offset..end).contains(&ph_offset)
}
