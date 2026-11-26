// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Eager loading of a kernel-owned ELF image without a VFS file.
//!
//! Header and program-header parsing delegates to the shared
//! `kernel_elf_parser` (which wraps `xmas_elf`).  Policy checks
//! that are specific to in-memory TA images — no interpreter,
//! no dynamic-relocation table, W^X, entry executability, mapped-page
//! limit — stay in this module.

use alloc::{collections::BTreeMap, string::String, vec::Vec};

use kernel_elf_parser::{AuxEntry, AuxType, ElfHeadersBuilder};
use kerrno::{KError, KResult};
use khal::paging::{MappingFlags, PageSize};
use memaddr::{PAGE_SIZE_4K, VirtAddr};
use memspace::{MmSpace, VmRuntimeRef};
use xmas_elf::{
    header::{Class, Data, Machine as ElfMachine},
    program::Type as PhType,
};

use super::loader::{
    auxv_bytes_to_words, map_initial_stack_and_heap, mapping_flags, prepare_initial_stack,
};

// ---------------------------------------------------------------------------
// Constants that represent *policy*, not ELF layout constants.
// ---------------------------------------------------------------------------

/// Hard limit on program header count to bound the parse loop.
const MAX_PROGRAM_HEADERS: usize = 256;
/// Size of one ELF64 program-header entry in bytes.
const PROGRAM_HEADER_BYTES: usize = 56;
/// Hard limit on mapped 4 KiB pages (64 MiB of virtual address space).
const MAX_MAPPED_PAGES: usize = 16 * 1024;

struct Segment {
    start: usize,
    file_offset: usize,
    file_size: usize,
}

struct MemElfImage {
    segments: Vec<Segment>,
    pages: BTreeMap<usize, MappingFlags>,
    entry: VirtAddr,
    phdr: usize,
    phnum: usize,
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn page_align_up(value: usize) -> KResult<usize> {
    value
        .checked_add(PAGE_SIZE_4K - 1)
        .map(|end| end & !(PAGE_SIZE_4K - 1))
        .ok_or(KError::InvalidExecutable)
}

fn checked_usize(value: u64) -> KResult<usize> {
    usize::try_from(value).map_err(|_| KError::InvalidExecutable)
}

fn try_add(base: usize, offset: usize) -> KResult<usize> {
    base.checked_add(offset).ok_or(KError::InvalidExecutable)
}

// ---------------------------------------------------------------------------
// Parse + validate (header done by kernel_elf_parser; policy stays here)
// ---------------------------------------------------------------------------

#[expect(
    clippy::too_many_lines,
    reason = "CG-028-focused-functions: ELF header, program-header, segment and W^X policy checks \
              share one parsed image and must stay in a single ordered pass"
)]
fn parse_and_validate(image: &[u8]) -> KResult<MemElfImage> {
    let builder = ElfHeadersBuilder::new(image).map_err(|_| KError::InvalidExecutable)?;
    let h = builder.header();

    // --- ELF ident & target checks ----------------------------------------
    if h.pt1.class() != Class::SixtyFour
        || h.pt1.data() != Data::LittleEndian
        || h.pt2.header_size() != 64
        || h.pt2.ph_entry_size() as usize != PROGRAM_HEADER_BYTES
        || h.pt2.machine().as_machine() != ElfMachine::AArch64
    {
        return Err(KError::InvalidExecutable);
    }

    // --- Type / base-address policy ---------------------------------------
    let e_type = h.pt2.type_().as_type();
    let base = match e_type {
        xmas_elf::header::Type::Executable => 0,
        xmas_elf::header::Type::SharedObject => kaddr_layout::USER_SPACE_BASE,
        _ => return Err(KError::InvalidExecutable),
    };

    // --- Program-header table bounds --------------------------------------
    let phnum = h.pt2.ph_count() as usize;
    if phnum == 0 || phnum > MAX_PROGRAM_HEADERS {
        return Err(KError::InvalidExecutable);
    }
    let phoff = h.pt2.ph_offset();
    if phoff > u64::MAX - (PROGRAM_HEADER_BYTES as u64 * phnum as u64) {
        return Err(KError::InvalidExecutable);
    }
    let phoff = checked_usize(phoff)?;
    let phend = phoff + phnum * PROGRAM_HEADER_BYTES; // bounded by the check above
    if phend > image.len() {
        return Err(KError::InvalidExecutable);
    }

    let ph_bytes = &image[phoff..phend];

    // --- Entry point ------------------------------------------------------
    let entry = try_add(base, checked_usize(h.pt2.entry_point())?)?;

    // --- Build shared parsed headers --------------------------------------
    let elf_headers = builder
        .build(ph_bytes)
        .map_err(|_| KError::InvalidExecutable)?;

    // --- Iterate program headers: policy + segment / page tracking --------
    let mut segments = Vec::new();
    let mut pages: BTreeMap<usize, MappingFlags> = BTreeMap::new();
    let mut is_entry_executable = false;
    // Only a mapped PT_LOAD containing the whole table may publish AT_PHDR.
    let mut phdr = None;

    for ph in &elf_headers.ph {
        let phtype = match ph.get_type() {
            Ok(t) => t,
            Err(_) => return Err(KError::InvalidExecutable),
        };

        match phtype {
            PhType::Interp => return Err(KError::Unsupported),
            PhType::Dynamic => return Err(KError::Unsupported),
            PhType::Load => { /* handled below */ }
            _ => continue,
        }

        // Reject unknown p_flags bits (only R=4, W=2, X=1 are valid).
        if ph.flags.0 & !7 != 0 {
            return Err(KError::InvalidExecutable);
        }

        // --- Field translation with overflow checks -----------------------
        let file_offset = checked_usize(ph.offset)?;
        let start = try_add(base, checked_usize(ph.virtual_addr)?)?;
        let file_sz = checked_usize(ph.file_size)?;
        let mem_sz = checked_usize(ph.mem_size)?;
        let file_end = file_offset
            .checked_add(file_sz)
            .ok_or(KError::InvalidExecutable)?;

        if file_sz > mem_sz || file_end > image.len() {
            return Err(KError::InvalidExecutable);
        }
        if mem_sz == 0 {
            continue;
        }

        let flags = mapping_flags(ph.flags);
        if flags == MappingFlags::USER {
            return Err(KError::InvalidExecutable);
        }

        is_entry_executable |= flags.contains(MappingFlags::EXECUTE)
            && (start..try_add(start, file_sz)?).contains(&entry);

        if file_offset <= phoff && phend <= file_end {
            phdr = Some(try_add(start, phoff - file_offset)?);
        }

        // --- Page map for later merging and W^X check ---------------------
        let page_start = start & !(PAGE_SIZE_4K - 1);
        let page_end = page_align_up(try_add(start, mem_sz)?)?;
        for page in (page_start..page_end).step_by(PAGE_SIZE_4K) {
            let pf = pages.entry(page).or_insert(MappingFlags::USER);
            *pf |= flags;
            if pages.len() > MAX_MAPPED_PAGES {
                return Err(KError::FileTooLarge);
            }
        }

        segments.push(Segment {
            start,
            file_offset,
            file_size: file_sz,
        });
    }

    // --- Final policy checks ----------------------------------------------
    if !is_entry_executable || segments.is_empty() {
        return Err(KError::InvalidExecutable);
    }
    if pages
        .values()
        .any(|f| f.contains(MappingFlags::WRITE) && f.contains(MappingFlags::EXECUTE))
    {
        return Err(KError::InvalidExecutable);
    }

    Ok(MemElfImage {
        segments,
        pages,
        entry: VirtAddr::from_usize(entry),
        phdr: phdr.ok_or(KError::InvalidExecutable)?,
        phnum,
    })
}

// ---------------------------------------------------------------------------
// Mapping helpers
// ---------------------------------------------------------------------------

impl MemElfImage {
    fn map_segments(&self, uspace: &mut MmSpace, image: &[u8]) -> KResult {
        // Merge contiguous same-flags runs and map them.
        let mut run_start = None;
        let mut run_end = 0;
        let mut run_flags = MappingFlags::empty();
        for (&page, &flags) in &self.pages {
            if run_start.is_some() && page == run_end && flags == run_flags {
                run_end += PAGE_SIZE_4K;
                continue;
            }
            if let Some(start) = run_start {
                map_run(uspace, start, run_end, run_flags)?;
            }
            run_start = Some(page);
            run_end = page + PAGE_SIZE_4K;
            run_flags = flags;
        }
        if let Some(start) = run_start {
            map_run(uspace, start, run_end, run_flags)?;
        }

        // Copy file-backed data into the fresh anonymous pages.
        for segment in &self.segments {
            if segment.file_size > 0 {
                let bytes = &image[segment.file_offset..segment.file_offset + segment.file_size];
                uspace.write(VirtAddr::from_usize(segment.start), bytes)?;
            }
        }
        Ok(())
    }

    fn auxv(&self) -> [AuxEntry; 5] {
        [
            AuxEntry::new(AuxType::PHDR, self.phdr),
            AuxEntry::new(AuxType::PHENT, PROGRAM_HEADER_BYTES),
            AuxEntry::new(AuxType::PHNUM, self.phnum),
            AuxEntry::new(AuxType::PAGESZ, PAGE_SIZE_4K),
            AuxEntry::new(AuxType::ENTRY, self.entry.as_usize()),
        ]
    }
}

fn map_run(uspace: &mut MmSpace, start: usize, end: usize, flags: MappingFlags) -> KResult {
    let vaddr = VirtAddr::from_usize(start);
    uspace.map(
        vaddr,
        end - start,
        flags,
        true,
        VmRuntimeRef::new_anon_private(vaddr, PageSize::Size4K),
    )
}

// ---------------------------------------------------------------------------
// Public entry point
// ---------------------------------------------------------------------------

/// Loads a static AArch64 ELF image held in kernel memory into an empty user
/// address space.  The source image is only read; mapped pages own their
/// copied contents and remain valid after the source buffer is released.
///
/// The caller supplies an immutable image already accepted by its TA upload
/// policy.  This loader validates ELF layout, rejects an external interpreter
/// and dynamic-relocation table, and enforces W^X.
/// Errors may leave partial mappings in `uspace`; discard it on failure.
pub fn load_user_elf_image(
    uspace: &mut MmSpace,
    image: &[u8],
    args: &[String],
    envs: &[String],
) -> KResult<(VirtAddr, VirtAddr, Vec<u64>)> {
    // Parse and validate before creating any mappings.
    let parsed = parse_and_validate(image)?;
    // Give the process a return path from signal handlers.
    ksignal::map_signal_trampoline(uspace)?;
    // Copy loadable segments into the process's own pages.
    parsed.map_segments(uspace, image)?;
    // Build the initial stack and heap with ELF auxiliary entries.
    let execfn = args.first().map(String::as_str).unwrap_or("/tee/ta");
    let (user_sp, stack_data, auxv_bytes) =
        prepare_initial_stack(args, envs, &parsed.auxv(), execfn)?;
    map_initial_stack_and_heap(uspace, user_sp, &stack_data)?;
    let saved_aux = auxv_bytes_to_words(&auxv_bytes);
    Ok((parsed.entry, user_sp, saved_aux))
}

// ---------------------------------------------------------------------------
// Unit tests
// ---------------------------------------------------------------------------

unittest::path_mod_test!(tests, "../ut-tests/elf_mem_loader.rs");
