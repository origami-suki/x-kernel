// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! ELF information parsed from the ELF file

use alloc::vec::Vec;
use core::ops::Range;

use xmas_elf::{
    header::Class,
    program::{ProgramHeader32, ProgramHeader64},
};

use crate::auxv::{AuxEntry, AuxType};

/// Incremental builder for [`ELFHeaders`].
///
/// The program header table usually lives outside the first block a loader
/// reads, so the builder is created from the ELF header alone, [`Self::ph_range`]
/// reports the bytes still needed, and [`Self::build`] parses them.
pub struct ELFHeadersBuilder<'a>(ELFHeaders<'a>);
impl<'a> ELFHeadersBuilder<'a> {
    /// Parses the ELF header at the start of `input`.
    ///
    /// # Errors
    ///
    /// Returns the parser's message when `input` does not start with a
    /// supported ELF header.
    pub fn new(input: &'a [u8]) -> Result<Self, &'static str> {
        Ok(Self(ELFHeaders {
            header: xmas_elf::header::parse_header(input)?,
            ph: Vec::new(),
        }))
    }

    /// Byte range of the program header table inside the file.
    ///
    /// The range comes from file-derived fields, so it is validated rather than
    /// trusted: an overflowing or non-representable table is rejected here
    /// instead of reaching a slicing or iteration panic later. The range is not
    /// clamped to the file size; the caller decides whether to read more bytes
    /// or reject the image.
    ///
    /// # Errors
    ///
    /// Returns a message when the entry size times the entry count, or that
    /// product added to the table offset, is not representable in `u64`.
    pub fn ph_range(&self) -> Result<Range<u64>, &'static str> {
        let start = self.0.header.pt2.ph_offset();
        let entry_size = self.0.header.pt2.ph_entry_size() as u64;
        let size = entry_size
            .checked_mul(self.0.header.pt2.ph_count() as u64)
            .ok_or("program header table size overflow")?;
        let end = start
            .checked_add(size)
            .ok_or("program header table range overflow")?;
        Ok(start..end)
    }

    /// Parses `ph` as the program header table and finishes the headers.
    ///
    /// `ph` must hold at least the range reported by [`Self::ph_range`]. Entries
    /// past the supplied bytes are dropped by the fixed-size chunking, matching
    /// a caller that read exactly the requested range.
    ///
    /// # Errors
    ///
    /// Returns a message when the header declares a zero program header entry
    /// size, which cannot be chunked into entries.
    pub fn build(mut self, ph: &[u8]) -> Result<ELFHeaders<'a>, &'static str> {
        let entry_size = self.0.header.pt2.ph_entry_size() as usize;
        if entry_size == 0 {
            return Err("program header entry size is zero");
        }
        self.0.ph = ph
            .chunks_exact(entry_size)
            .map(|chunk| match self.0.header.pt1.class() {
                Class::ThirtyTwo => {
                    let ph: &ProgramHeader32 = zero::read(chunk);
                    ProgramHeader64 {
                        type_: ph.type_,
                        offset: ph.offset as _,
                        virtual_addr: ph.virtual_addr as _,
                        physical_addr: ph.physical_addr as _,
                        file_size: ph.file_size as _,
                        mem_size: ph.mem_size as _,
                        flags: ph.flags,
                        align: ph.align as _,
                    }
                }
                Class::SixtyFour => *zero::read(chunk),
                Class::None | Class::Other(_) => unreachable!(),
            })
            .collect();
        Ok(self.0)
    }
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

    /// The entry point of the ELF file.
    pub fn entry(&self) -> usize {
        // TODO: base_load_address_offset?
        self.headers.header.pt2.entry_point() as usize + self.base
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
    pub fn phdr(&self) -> usize {
        let ph_offset = self.headers.header.pt2.ph_offset() as usize;
        let header = self
            .headers
            .ph
            .iter()
            .find(|header| {
                (header.offset..header.offset + header.file_size).contains(&(ph_offset as u64))
            })
            .expect("can not find program header table address in elf");
        ph_offset - header.offset as usize + header.virtual_addr as usize + self.base
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
    /// Details about auxiliary vectors are described in <https://articles.manugarg.com/aboutelfauxiliaryvectors.html>
    pub fn aux_vector(
        &self,
        pagesz: usize,
        ldso_base: Option<usize>,
    ) -> impl Iterator<Item = AuxEntry> {
        [
            (AuxType::PHDR, self.phdr()),
            (AuxType::PHENT, self.phent()),
            (AuxType::PHNUM, self.phnum()),
            (AuxType::PAGESZ, pagesz),
            (AuxType::ENTRY, self.entry()),
        ]
        .into_iter()
        .chain(ldso_base.into_iter().map(|base| (AuxType::BASE, base)))
        .map(|(at, val)| AuxEntry::new(at, val))
    }
}
