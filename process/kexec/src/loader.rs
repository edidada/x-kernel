// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! ELF loading for user programs.

use alloc::{borrow::ToOwned, string::String, vec, vec::Vec};
use core::{ffi::CStr, iter, ops::Range};

use kernel_elf_parser::{AuxEntry, ELFHeaders, ELFHeadersBuilder, ELFParser, app_stack_region};
use kerrno::{KError, KResult};
use kfs::{CachedFile, FileBackend};
use khal::paging::{MappingFlags, PageSize};
use ksync::Mutex;
use kvfs::Location;
use memaddr::{MemoryAddr, PAGE_SIZE_4K, VirtAddr};
use memspace::AddrSpace;
use memspace_file::{new_alloc, new_cow};
use ouroboros::self_referencing;

use super::{at_random::fill_at_random, lru_cache::LruCache};

fn mapping_flags(flags: xmas_elf::program::Flags) -> MappingFlags {
    let mut mapping_flags = MappingFlags::USER;
    if flags.is_read() {
        mapping_flags |= MappingFlags::READ;
    }
    if flags.is_write() {
        mapping_flags |= MappingFlags::WRITE;
    }
    if flags.is_execute() {
        mapping_flags |= MappingFlags::EXECUTE;
    }
    mapping_flags
}

fn is_load_seg_page_offset_consistent(vaddr: usize, file_offset: u64) -> bool {
    vaddr.align_offset_4k() == file_offset as usize % PAGE_SIZE_4K
}

fn map_elf<'a>(
    uspace: &mut AddrSpace,
    base: usize,
    entry: &'a ElfCacheEntry,
) -> KResult<ELFParser<'a>> {
    let elf_parser = ELFParser::new(entry.borrow_elf(), base).map_err(|_| KError::InvalidData)?;
    let cache = entry.borrow_cache();

    for ph in elf_parser
        .headers()
        .ph
        .iter()
        .filter(|ph| ph.get_type() == Ok(xmas_elf::program::Type::Load))
    {
        let vaddr = ph.virtual_addr as usize + elf_parser.base();
        debug!(
            "Mapping ELF segment: [{:#x?}, {:#x?}) flags: {}",
            vaddr,
            vaddr + ph.mem_size as usize,
            ph.flags
        );
        let seg_pad = vaddr.align_offset_4k();
        // PT_LOAD p_vaddr and p_offset must share the same page offset so the
        // mapping can be page-aligned. A mismatch is a malformed ELF from
        // execve, not an internal invariant: return ENOEXEC instead of panicking.
        if !is_load_seg_page_offset_consistent(vaddr, ph.offset) {
            return Err(KError::InvalidExecutable);
        }

        let seg_align_size =
            (ph.mem_size as usize + seg_pad + PAGE_SIZE_4K - 1) & !(PAGE_SIZE_4K - 1);
        let seg_start = VirtAddr::from_usize(vaddr);

        // Note that `offset` might not be aligned to 4K here, and it is the
        // backend's responsibility to handle it.
        let backend = new_cow(
            seg_start,
            PageSize::Size4K,
            FileBackend::Cached(cache.clone()),
            ph.offset,
            Some(ph.offset + ph.file_size),
        );
        uspace.map(
            seg_start.align_down_4k(),
            seg_align_size,
            mapping_flags(ph.flags),
            false,
            backend,
        )?;
    }

    Ok(elf_parser)
}

fn map_elf_error(err: &'static str) -> KError {
    debug!("Failed to parse ELF file: {err}");
    KError::InvalidExecutable
}

/// Slice the program-header table out of an already-read prefix.
///
/// `slice.get(start..end)` returns `None` when the range is inverted or
/// out of bounds, so a wrapped `e_phoff` cannot panic here.
fn ph_table_in_prefix<'a>(data: &'a [u8], range: &Range<u64>) -> Option<&'a [u8]> {
    let start = usize::try_from(range.start).ok()?;
    let end = usize::try_from(range.end).ok()?;
    data.get(start..end)
}

fn ph_table_file_span(range: &Range<u64>) -> KResult<(u64, usize)> {
    let len = range
        .end
        .checked_sub(range.start)
        .ok_or(KError::InvalidExecutable)?;
    let len = usize::try_from(len).map_err(|_| KError::InvalidExecutable)?;
    Ok((range.start, len))
}

#[self_referencing]
struct ElfCacheEntry {
    cache: CachedFile,
    data: Vec<u8>,
    #[borrows(data)]
    #[covariant]
    elf: ELFHeaders<'this>,
}

impl ElfCacheEntry {
    fn load(loc: Location) -> KResult<Result<Self, Vec<u8>>> {
        let cache = CachedFile::get_or_create(loc)?;

        let mut data = vec![0; 4096];
        let read = cache.read_at(&mut data[..], 0)?;
        data.truncate(read);
        match ElfCacheEntry::try_new_or_recover::<KError>(cache.clone(), data, |data| {
            let builder = ELFHeadersBuilder::new(data).map_err(map_elf_error)?;
            let range = builder.ph_range().map_err(map_elf_error)?;
            if let Some(ph) = ph_table_in_prefix(data, &range) {
                builder.build(ph)
            } else {
                let (offset, len) = ph_table_file_span(&range)?;
                let mut buf = vec![0; len];
                let read = cache.read_at(&mut buf[..], offset)?;
                if read != len {
                    return Err(KError::InvalidExecutable);
                }
                builder.build(&buf)
            }
            .map_err(map_elf_error)
        }) {
            Ok(entry) => {
                #[cfg(feature = "tee_ta_sign")]
                {
                    tee_task_iface::tasign::verify_ta_elf_on_load_and_cache_ta_head(
                        entry.borrow_cache(),
                    )
                    .map_err(|_err| KError::PermissionDenied)?;
                }
                Ok(Ok(entry))
            }
            Err((_, heads)) => Ok(Err(heads.data)),
        }
    }
}

struct ElfLoader(LruCache<ElfCacheEntry, 32>);

type LoadResult = Result<(VirtAddr, Vec<AuxEntry>), Vec<u8>>;

impl ElfLoader {
    const fn new() -> Self {
        Self(LruCache::new())
    }

    fn load(&mut self, uspace: &mut AddrSpace, path: &str) -> KResult<LoadResult> {
        let loc = kthread::current_fs_context().lock().resolve(path)?;
        self.load_location(uspace, loc)
    }

    fn load_location(&mut self, uspace: &mut AddrSpace, loc: Location) -> KResult<LoadResult> {
        if !self
            .0
            .access(|entry| entry.borrow_cache().location().ptr_eq(&loc))
        {
            match ElfCacheEntry::load(loc.clone())? {
                Ok(entry) => {
                    self.0.put(entry);
                }
                Err(data) => {
                    return Ok(Err(data));
                }
            }
        }

        uspace.clear();
        ksignal::map_signal_trampoline(uspace)?;

        let ldso_loc: Option<Location> = {
            let entry = self.0.peek_mru().ok_or(KError::InvalidExecutable)?;
            if let Some(header) = entry
                .borrow_elf()
                .ph
                .iter()
                .find(|ph| ph.get_type() == Ok(xmas_elf::program::Type::Interp))
            {
                let cache = entry.borrow_cache();
                let mut data = vec![0; header.file_size as usize];
                let read = cache.read_at(&mut data[..], header.offset)?;
                // `read_at` returns a short count when the file ends early. A
                // PT_INTERP `p_filesz` larger than the file is a malformed ELF,
                // not an internal invariant.
                if read != data.len() {
                    return Err(KError::InvalidExecutable);
                }

                let ldso = CStr::from_bytes_with_nul(&data)
                    .ok()
                    .and_then(|cstr| cstr.to_str().ok())
                    .ok_or(KError::InvalidInput)?;
                debug!("Loading dynamic linker: {ldso}");
                Some(kthread::current_fs_context().lock().resolve(ldso)?)
            } else {
                None
            }
        };

        if let Some(ref ldso_loc) = ldso_loc {
            if ldso_loc.ptr_eq(&loc) {
                return Err(KError::InvalidExecutable);
            }
            if !self
                .0
                .access(|entry| entry.borrow_cache().location().ptr_eq(ldso_loc))
            {
                match ElfCacheEntry::load(ldso_loc.clone())? {
                    Ok(entry) => {
                        self.0.put(entry);
                    }
                    Err(data) => {
                        return Ok(Err(data));
                    }
                }
            }
        }

        let elf_entry = self
            .0
            .items()
            .find(|entry| entry.borrow_cache().location().ptr_eq(&loc))
            .ok_or(KError::InvalidExecutable)?;
        let ldso_entry = ldso_loc.as_ref().and_then(|ldso_loc| {
            self.0
                .items()
                .find(|entry| entry.borrow_cache().location().ptr_eq(ldso_loc))
        });

        let elf = map_elf(uspace, kaddr_layout::USER_SPACE_BASE, elf_entry)?;
        let ldso = ldso_entry
            .map(|entry| map_elf(uspace, kaddr_layout::USER_INTERP_BASE, entry))
            .transpose()?;

        let entry_va = ldso
            .as_ref()
            .map_or_else(|| elf.entry(), |ldso| ldso.entry())
            .map_err(|_| KError::InvalidData)?;
        let entry = VirtAddr::from_usize(entry_va);
        let auxv = elf
            .aux_vector(PAGE_SIZE_4K, ldso.map(|elf| elf.base()))
            .map_err(|_| KError::InvalidData)?
            .collect::<Vec<_>>();

        Ok(Ok((entry, auxv)))
    }
}

static ELF_LOADER: Mutex<ElfLoader> = Mutex::new(ElfLoader::new());

/// Linux `execve(2)` allows at most four interpreter rewrites.
///
/// The `#!` parser and the `.sh` suffix rewrite share this budget so a
/// cyclic script chain cannot overflow the kernel stack.
const MAX_INTERP_DEPTH: usize = 4;

/// Clear the ELF cache.
///
/// Useful for removing noise during memory leak detection.
pub fn clear_elf_cache() {
    ELF_LOADER.lock().0.flush();
    #[cfg(feature = "tee_ta_sign")]
    tee_task_iface::tasign::clear_ta_head_cache();
}

/// Load the user app to the user address space.
///
/// Script files may rewrite to an interpreter. Nested `#!` lines and
/// the `.sh` suffix rewrite share a depth budget of four, matching
/// Linux `execve(2)`.
///
/// # Arguments
///
/// - `uspace`: The address space of the user app.
/// - `path`: Optional executable path. When omitted, the first argument
///   is used.
/// - `args`: The arguments of the user app. The first argument is the path of
///   the user app when `path` is omitted.
/// - `envs`: The environment variables of the user app.
///
/// # Returns
///
/// - The entry point of the user app.
/// - The stack pointer of the user app.
///
/// # Errors
///
/// Returns [`KError::FilesystemLoop`] if interpreter rewriting nests more
/// than four times, including cyclic `#!` references.
pub fn load_user_app(
    uspace: &mut AddrSpace,
    path: Option<&str>,
    args: &[String],
    envs: &[String],
) -> KResult<(VirtAddr, VirtAddr)> {
    load_user_app_resolved(uspace, None, path, args, envs, 0)
}

/// Load the user app from an already-resolved filesystem location.
///
/// Interpreter rewriting follows the same four-level budget as
/// [`load_user_app`].
///
/// # Errors
///
/// Returns [`KError::FilesystemLoop`] if interpreter rewriting nests more
/// than four times, including cyclic `#!` references.
pub fn load_user_app_at(
    uspace: &mut AddrSpace,
    loc: Location,
    path: &str,
    args: &[String],
    envs: &[String],
) -> KResult<(VirtAddr, VirtAddr)> {
    load_user_app_resolved(uspace, Some(loc), Some(path), args, envs, 0)
}

fn next_interp_depth(interp_depth: usize) -> KResult<usize> {
    if interp_depth >= MAX_INTERP_DEPTH {
        Err(KError::FilesystemLoop)
    } else {
        Ok(interp_depth + 1)
    }
}

fn load_next_interp(
    uspace: &mut AddrSpace,
    args: &[String],
    envs: &[String],
    interp_depth: usize,
) -> KResult<(VirtAddr, VirtAddr)> {
    let interp_depth = next_interp_depth(interp_depth)?;
    load_user_app_resolved(uspace, None, None, args, envs, interp_depth)
}

fn load_user_app_resolved(
    uspace: &mut AddrSpace,
    loc: Option<Location>,
    path: Option<&str>,
    args: &[String],
    envs: &[String],
    interp_depth: usize,
) -> KResult<(VirtAddr, VirtAddr)> {
    let path = path
        .or_else(|| args.first().map(String::as_str))
        .ok_or(KError::InvalidInput)?;

    // FIXME: Implement `/proc/self/exe` to let busybox retry running scripts.
    if path.ends_with(".sh") {
        let new_args: Vec<String> = iter::once("/bin/sh".to_owned())
            .chain(args.iter().cloned())
            .collect();
        return load_next_interp(uspace, &new_args, envs, interp_depth);
    }

    let load_result = {
        let mut loader = ELF_LOADER.lock();
        match loc {
            Some(loc) => loader.load_location(uspace, loc)?,
            None => loader.load(uspace, path)?,
        }
    };

    let (entry, auxv) = match load_result {
        Ok((entry, auxv)) => (entry, auxv),
        Err(data) => {
            if data.starts_with(b"#!") {
                let head = &data[2..data.len().min(256)];
                let pos = head
                    .iter()
                    .position(|byte| *byte == b'\n')
                    .unwrap_or(head.len());
                let line = core::str::from_utf8(&head[..pos]).map_err(|_| KError::InvalidInput)?;

                let new_args: Vec<String> = line
                    .trim()
                    .splitn(2, |c: char| c.is_ascii_whitespace())
                    .map(|s| s.trim_ascii().to_owned())
                    .chain(iter::once(path.to_owned()))
                    .chain(args.iter().skip(1).cloned())
                    .collect();
                return load_next_interp(uspace, &new_args, envs, interp_depth);
            }
            return Err(KError::InvalidExecutable);
        }
    };

    let ustack_top = VirtAddr::from_usize(kaddr_layout::USER_STACK_TOP);
    let ustack_size = kaddr_layout::USER_STACK_SIZE;
    let ustack_start = ustack_top - ustack_size;
    debug!("Mapping user stack: {ustack_start:#x?} -> {ustack_top:#x?}");

    uspace.map(
        ustack_start,
        ustack_size,
        MappingFlags::READ | MappingFlags::WRITE | MappingFlags::USER,
        false,
        new_alloc(ustack_start, PageSize::Size4K),
    )?;

    let mut at_random = [0u8; 16];
    fill_at_random(&mut at_random);
    let stack_data = app_stack_region(args, envs, &auxv, &at_random, ustack_top.into())
        .map_err(|_| KError::ArgumentListTooLong)?;
    if stack_data.len() > ustack_size {
        return Err(KError::ArgumentListTooLong);
    }
    let user_sp = ustack_top - stack_data.len();
    let user_sp_aligned = user_sp.align_down_4k();
    uspace.populate_area(
        user_sp_aligned,
        (ustack_top - user_sp_aligned).align_up_4k(),
        MappingFlags::READ | MappingFlags::WRITE,
    )?;
    uspace.write(user_sp, stack_data.as_slice())?;

    let heap_start = VirtAddr::from_usize(kaddr_layout::USER_HEAP_BASE);
    let heap_size = kaddr_layout::USER_HEAP_SIZE;
    uspace.map(
        heap_start,
        heap_size,
        MappingFlags::READ | MappingFlags::WRITE | MappingFlags::USER,
        true,
        new_alloc(heap_start, PageSize::Size4K),
    )?;

    Ok((entry, user_sp))
}

#[cfg(unittest)]
mod tests {
    use khal::paging::MappingFlags;
    use unittest::def_test;
    use xmas_elf::program::{FLAG_R, FLAG_W, FLAG_X, Flags};

    use super::mapping_flags;

    #[def_test]
    fn test_mapping_flags_sets_user_and_requested_permissions() {
        let none = mapping_flags(Flags(0));
        assert_eq!(none, MappingFlags::USER);

        let read = mapping_flags(Flags(FLAG_R));
        assert!(read.contains(MappingFlags::USER | MappingFlags::READ));
        assert!(!read.contains(MappingFlags::WRITE));

        let write_exec = mapping_flags(Flags(FLAG_W | FLAG_X));
        assert!(write_exec.contains(MappingFlags::USER | MappingFlags::WRITE));
        assert!(write_exec.contains(MappingFlags::EXECUTE));
        assert!(!write_exec.contains(MappingFlags::READ));
    }

    #[def_test]
    fn test_mapping_flags_all_bits_combination() {
        let flags = mapping_flags(Flags(FLAG_R | FLAG_W | FLAG_X));
        assert!(flags.contains(MappingFlags::USER));
        assert!(flags.contains(MappingFlags::READ));
        assert!(flags.contains(MappingFlags::WRITE));
        assert!(flags.contains(MappingFlags::EXECUTE));
    }
}
