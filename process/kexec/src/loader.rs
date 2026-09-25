// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! ELF loading for user programs.

use alloc::{borrow::ToOwned, string::String, sync::Arc, vec, vec::Vec};
use core::{ffi::CStr, iter};

use filemap::new_file_private_vma;
use kcred::Cred;
use kernel_elf_parser::{AuxEntry, ELFHeaders, ELFHeadersBuilder, ELFParser, app_stack_region};
use kerrno::{KError, KResult};
use khal::paging::{MappingFlags, PageSize};
use ksync::{Mutex, static_lock};
use kvfs::{Filename, LookupFlags, LookupIntent, Path, Permission, VfsFile, dentry_open};
use memaddr::{MemoryAddr, PAGE_SIZE_4K, VirtAddr, VirtAddrRange};
use memspace::{MmSpace, VmRuntimeRef};
use ouroboros::self_referencing;

use super::lru_cache::LruCache;

const SCRIPT_RECURSION_MAX: usize = 4;

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

fn open_exec_file(location: &Path, cred: Arc<Cred>) -> KResult<Arc<VfsFile>> {
    dentry_open(location.clone(), 0, cred)
}

/// Source of an executable image.
#[derive(Clone)]
pub enum ExecSource {
    /// Resolve the executable from the caller's current filesystem context.
    Path(String),
    /// Use an already-resolved VFS location as the executable.
    Resolved {
        /// Resolved executable location.
        location: Path,
        /// Display path used for process metadata and script argv rewriting.
        display_path: Option<String>,
    },
}

impl ExecSource {
    fn resolve(&self, cred: &Cred) -> KResult<(Path, String)> {
        match self {
            Self::Path(path) => {
                let fs_struct = kprocess::current_fs_context();
                let fs = fs_struct.lock();
                let location = Filename::new(path.as_str()).lookup_at(
                    fs.root(),
                    fs.pwd(),
                    LookupIntent::Exec,
                    LookupFlags::follow(),
                    cred,
                )?;
                location.permission(Permission::MAY_EXEC, cred)?;
                Ok((location, path.clone()))
            }
            Self::Resolved {
                location,
                display_path,
            } => {
                location.permission(Permission::MAY_EXEC, cred)?;
                let display_path = match display_path {
                    Some(path) => path.clone(),
                    None => location.absolute_path()?.as_str().to_owned(),
                };
                Ok((location.clone(), display_path))
            }
        }
    }
}

/// Owned exec request before executable resolution.
pub struct ExecRequest {
    source: ExecSource,
    args: Vec<String>,
    envs: Vec<String>,
    cred: Arc<Cred>,
}

impl ExecRequest {
    /// Creates an exec request from a path string.
    pub fn from_path(
        path: impl Into<String>,
        args: Vec<String>,
        envs: Vec<String>,
        cred: Arc<Cred>,
    ) -> Self {
        Self {
            source: ExecSource::Path(path.into()),
            args,
            envs,
            cred,
        }
    }

    /// Creates an exec request from an already-resolved executable location.
    pub fn from_resolved(
        location: Path,
        args: Vec<String>,
        envs: Vec<String>,
        cred: Arc<Cred>,
    ) -> Self {
        Self {
            source: ExecSource::Resolved {
                location,
                display_path: None,
            },
            args,
            envs,
            cred,
        }
    }

    /// Creates an exec request from a resolved executable plus display path.
    pub fn from_resolved_with_display(
        location: Path,
        display_path: String,
        args: Vec<String>,
        envs: Vec<String>,
        cred: Arc<Cred>,
    ) -> Self {
        Self {
            source: ExecSource::Resolved {
                location,
                display_path: Some(display_path),
            },
            args,
            envs,
            cred,
        }
    }

    /// Returns the requested executable source.
    pub fn source(&self) -> &ExecSource {
        &self.source
    }

    /// Returns the owned argument vector.
    pub fn args(&self) -> &[String] {
        &self.args
    }

    /// Returns the owned environment vector.
    pub fn envs(&self) -> &[String] {
        &self.envs
    }

    /// Resolves the executable and creates a binprm object without mutating
    /// the target address space.
    ///
    /// # Errors
    ///
    /// Propagates VFS lookup, execute-permission, absolute-path reconstruction, and
    /// file-open errors. Both resolved and string sources undergo an execute
    /// permission check using the supplied credential snapshot. No address space is modified.
    pub fn prepare(self) -> KResult<BinPrm> {
        let (location, display_path) = self.source.resolve(&self.cred)?;
        let executable = open_exec_file(&location, self.cred.clone())?;
        Ok(BinPrm {
            location,
            executable,
            display_path,
            args: self.args,
            envs: self.envs,
            cred: self.cred,
        })
    }
}

/// Prepared executable image state.
pub struct BinPrm {
    location: Path,
    executable: Arc<VfsFile>,
    display_path: String,
    args: Vec<String>,
    envs: Vec<String>,
    cred: Arc<Cred>,
}

impl BinPrm {
    /// Returns the executable location.
    pub fn location(&self) -> &Path {
        &self.location
    }

    /// Returns the opened executable file.
    pub fn executable(&self) -> &Arc<VfsFile> {
        &self.executable
    }

    fn cred(&self) -> &Arc<Cred> {
        &self.cred
    }

    /// Returns the display path used for argv/script reconstruction.
    pub fn display_path(&self) -> &str {
        &self.display_path
    }

    /// Returns the owned argument vector.
    pub fn args(&self) -> &[String] {
        &self.args
    }

    /// Returns the owned environment vector.
    pub fn envs(&self) -> &[String] {
        &self.envs
    }
}

struct PreparedExecImage {
    binprm: BinPrm,
    interpreter: Option<Arc<VfsFile>>,
}

fn map_elf<'a>(
    uspace: &mut MmSpace,
    base: usize,
    entry: &'a ElfCacheEntry,
) -> KResult<ELFParser<'a>> {
    let elf_parser = ELFParser::new(entry.borrow_elf(), base).map_err(|_| KError::InvalidData)?;
    let file = entry.borrow_file();

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
        if seg_pad != ph.offset as usize % PAGE_SIZE_4K {
            warn!(
                "PT_LOAD page offset mismatch: vaddr={vaddr:#x} p_offset={:#x}",
                ph.offset
            );
            return Err(KError::InvalidExecutable);
        }

        let seg_align_size =
            (ph.mem_size as usize + seg_pad + PAGE_SIZE_4K - 1) & !(PAGE_SIZE_4K - 1);
        let seg_start = VirtAddr::from_usize(vaddr);
        let mapped_start = seg_start.align_down_4k();
        let file_start = (ph.offset as usize).align_down_4k() as u64;

        // PT_LOAD mappings follow the Linux rule that both VMA start and file
        // offset are aligned down to the page boundary. The page prefix before
        // `p_vaddr` still belongs to the mapped file object and must not be
        // silently zero-filled.
        let flags = mapping_flags(ph.flags);
        let (vma, runtime) = new_file_private_vma(
            mapped_start,
            seg_align_size,
            PageSize::Size4K,
            file.clone(),
            file_start,
            Some(ph.offset + ph.file_size),
            flags,
        )?;
        uspace.map_runtime_vma(vma, false, runtime)?;
    }

    Ok(elf_parser)
}

/// Virtual placement facts of one loadable ELF segment.
#[derive(Clone, Copy)]
struct LoadSegment {
    virtual_addr: usize,
    mem_size: usize,
    align: usize,
}

/// Returns the loadable segments of `entry` with their placement facts.
fn load_segments(entry: &ElfCacheEntry, base: usize) -> Option<Vec<LoadSegment>> {
    let parser = ELFParser::new(entry.borrow_elf(), base).ok()?;
    Some(
        parser
            .headers()
            .ph
            .iter()
            .filter(|ph| ph.get_type() == Ok(xmas_elf::program::Type::Load))
            .map(|ph| LoadSegment {
                virtual_addr: ph.virtual_addr as usize,
                mem_size: ph.mem_size as usize,
                align: ph.align as usize,
            })
            .collect(),
    )
}

/// Range covered by `segments` when the image is loaded at `base`.
///
/// The result mirrors `map_elf` exactly: a segment maps
/// `align_up(mem_size + page_offset)` bytes starting at
/// `align_down(virtual_addr)`. The page offset must be added *before* rounding,
/// because rounding the length alone can omit a mapped tail page. The lengths
/// differ exactly when `align_up(mem_size + page_offset)` exceeds
/// `align_up(mem_size)`; neither the offset nor the length alone decides this.
///
/// Returns `None` when there is no loadable segment or when an end address
/// overflows.
fn load_range_for_segments(segments: &[LoadSegment], base: usize) -> Option<VirtAddrRange> {
    let mut start = None;
    let mut end = 0usize;

    for segment in segments {
        let vaddr = segment.virtual_addr.checked_add(base)?;
        let page_offset = vaddr.align_offset_4k();
        let mapped_start = VirtAddr::from_usize(vaddr).align_down_4k().as_usize();
        // Round up with checked arithmetic: `VirtAddr::align_up_4k` panics on
        // overflow, and both the page offset and the size come from the file.
        let unaligned_size = page_offset.checked_add(segment.mem_size)?;
        let mapped_size = unaligned_size.checked_add(PAGE_SIZE_4K - 1)? & !(PAGE_SIZE_4K - 1);
        let mapped_end = mapped_start.checked_add(mapped_size)?;
        start = Some(start.map_or(mapped_start, |current: usize| current.min(mapped_start)));
        end = end.max(mapped_end);
    }

    Some(VirtAddrRange::new(
        VirtAddr::from_usize(start?),
        VirtAddr::from_usize(end),
    ))
}

/// Bytes that must stay free for `segments` to be mapped at a chosen base.
///
/// `span` is computed with the preferred base, but the image does not start
/// there: its first mapped page sits `align_down(virtual_addr)` bytes above the
/// bias. The reserved region therefore has to start at the bias rather than at
/// the image's first page, so `offset(span.start, base)` must be covered too.
///
/// Reserving only `span.size()` lets the search return a region whose tail the
/// image then maps over: a main image that leaves one free page before an
/// occupied one makes the search report that page as usable, while the
/// interpreter's first segment maps into the occupied page.
///
/// Returns `None` when the span starts below `base` or the size overflows.
fn placement_reservation(span: VirtAddrRange, base: usize, align: usize) -> Option<usize> {
    let normalized_offset = span.start.as_usize().checked_sub(base)?;
    let unaligned = normalized_offset.checked_add(span.size())?;
    // Checked rounding: `align_up` panics on overflow, and both the offset and
    // the size come from the file.
    Some(unaligned.checked_add(align - 1)? & !(align - 1))
}

/// Chooses a free load bias for the dynamic interpreter.
///
/// The interpreter is position-independent, so its bias is a placement
/// decision rather than an ELF-provided constant: the fixed
/// `USER_INTERP_BASE` hint is only the preferred start. Reusing it
/// unconditionally collides with any main image whose PT_LOAD pages reach
/// that address, which used to fail the VMA non-overlap assertion and panic
/// the kernel.
///
/// The search stays inside the documented interpreter window below
/// `USER_HEAP_BASE`, so the heap and brk keep their fixed placement.
fn select_interpreter_base(uspace: &MmSpace, entry: &ElfCacheEntry) -> KResult<usize> {
    let hint = VirtAddr::from_usize(kaddr_layout::USER_INTERP_BASE);
    let limit = VirtAddrRange::new(hint, VirtAddr::from_usize(kaddr_layout::USER_HEAP_BASE));
    let segments = load_segments(entry, hint.as_usize()).ok_or(KError::InvalidExecutable)?;
    let span =
        load_range_for_segments(&segments, hint.as_usize()).ok_or(KError::InvalidExecutable)?;
    let align = segments
        .first()
        .map_or(PAGE_SIZE_4K, |segment| segment.align.max(PAGE_SIZE_4K));
    // `find_free_area` only reports a free start address that is itself
    // `align`-aligned, so the reserved size must cover the same rounding.
    let size =
        placement_reservation(span, hint.as_usize(), align).ok_or(KError::InvalidExecutable)?;

    uspace
        .find_free_area(hint, size, limit, align)
        .map(VirtAddr::as_usize)
        .ok_or(KError::NoMemory)
}

fn map_elf_error(err: &'static str) -> KError {
    debug!("Failed to parse ELF file: {err}");
    KError::InvalidExecutable
}

#[self_referencing]
struct ElfCacheEntry {
    file: Arc<VfsFile>,
    data: Vec<u8>,
    #[borrows(data)]
    #[covariant]
    elf: ELFHeaders<'this>,
}

impl ElfCacheEntry {
    fn load_file(file: Arc<VfsFile>) -> KResult<Result<Self, Vec<u8>>> {
        let mut data = vec![0; 4096];
        let mut pos = 0;
        let read = file.read_from(&mut data[..], &mut pos)?;
        data.truncate(read);
        match ElfCacheEntry::try_new_or_recover::<KError>(file.clone(), data, |data| {
            let builder = ELFHeadersBuilder::new(data).map_err(map_elf_error)?;
            let range = builder.ph_range().map_err(map_elf_error)?;
            if range.end as usize <= data.len() {
                builder.build(&data[range.start as usize..range.end as usize])
            } else {
                let mut buf = vec![0; (range.end - range.start) as usize];
                let mut pos = range.start;
                file.read_from(&mut buf[..], &mut pos)?;
                builder.build(&buf)
            }
            .map_err(map_elf_error)
        }) {
            Ok(entry) => {
                #[cfg(feature = "tee_ta_sign")]
                {
                    tee_task_iface::tasign::verify_ta_elf_on_load_and_cache_ta_head(
                        entry.borrow_file(),
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
type CacheProbeResult = Result<(), Vec<u8>>;
type PreparedImageResult = Result<PreparedExecImage, (BinPrm, Vec<u8>)>;

impl ElfLoader {
    const fn new() -> Self {
        Self(LruCache::new())
    }

    fn access_cached(&mut self, loc: &Path) -> bool {
        if !self
            .0
            .access(|entry| entry.borrow_file().path().ptr_eq(loc))
        {
            return false;
        }
        true
    }

    fn cached_entry(&self, loc: &Path) -> Option<&ElfCacheEntry> {
        self.0
            .items()
            .find(|entry| entry.borrow_file().path().ptr_eq(loc))
    }

    fn ensure_cached(&mut self, file: Arc<VfsFile>) -> KResult<CacheProbeResult> {
        if !self.access_cached(file.path()) {
            match ElfCacheEntry::load_file(file)? {
                Ok(entry) => {
                    self.0.put(entry);
                }
                Err(data) => return Ok(Err(data)),
            }
        }
        Ok(Ok(()))
    }

    fn interp_path(entry: &ElfCacheEntry) -> KResult<Option<String>> {
        let Some(header) = entry
            .borrow_elf()
            .ph
            .iter()
            .find(|ph| ph.get_type() == Ok(xmas_elf::program::Type::Interp))
        else {
            return Ok(None);
        };

        let file = entry.borrow_file();
        let mut data = vec![0; header.file_size as usize];
        let mut pos = header.offset;
        let read = file.read_from(&mut data[..], &mut pos)?;
        if read != data.len() {
            debug!("Short PT_INTERP read: want={} got={read}", data.len());
            return Err(KError::InvalidInput);
        }

        let ldso = CStr::from_bytes_with_nul(&data)
            .ok()
            .and_then(|cstr| cstr.to_str().ok())
            .ok_or(KError::InvalidInput)?;
        Ok(Some(ldso.to_owned()))
    }

    fn prepare_binprm(&mut self, binprm: BinPrm) -> KResult<PreparedImageResult> {
        match self.ensure_cached(binprm.executable().clone())? {
            Ok(_) => {}
            Err(data) => return Ok(Err((binprm, data))),
        }

        let interpreter = {
            let executable = self
                .cached_entry(binprm.location())
                .expect("executable entry must be cached before exec commit");
            Self::interp_path(executable)?
        };
        let interpreter = if let Some(ldso) = interpreter {
            debug!("Loading dynamic linker: {ldso}");
            let fs_struct = kprocess::current_fs_context();
            let fs = fs_struct.lock();
            let location = Filename::new(ldso.as_str()).lookup_at(
                fs.root(),
                fs.pwd(),
                LookupIntent::Exec,
                LookupFlags::follow(),
                binprm.cred(),
            )?;
            location.permission(Permission::MAY_EXEC, binprm.cred())?;
            let file = open_exec_file(&location, binprm.cred().clone())?;
            match self.ensure_cached(file.clone())? {
                Ok(_) => Some(file),
                Err(_) => return Err(KError::InvalidInput),
            }
        } else {
            None
        };
        Ok(Ok(PreparedExecImage {
            binprm,
            interpreter,
        }))
    }

    fn commit_prepared_binprm(
        &mut self,
        uspace: &mut MmSpace,
        prepared: &PreparedExecImage,
    ) -> KResult<(VirtAddr, Vec<AuxEntry>)> {
        // Point of no return: from here on the old user image is discarded and
        // all remaining work must consume prevalidated, already-pinned objects.
        uspace.clear();
        ksignal::map_signal_trampoline(uspace)?;

        let elf = self
            .cached_entry(prepared.binprm.location())
            .expect("prepared executable entry must remain cached while loading");
        let ldso = prepared.interpreter.as_ref().map(|file| {
            self.cached_entry(file.path())
                .expect("prepared interpreter entry must remain cached while loading")
        });

        let elf = map_elf(uspace, kaddr_layout::USER_SPACE_BASE, elf)?;
        let ldso = ldso
            .map(|entry| {
                let base = select_interpreter_base(uspace, entry)?;
                debug!("Mapping interpreter at base {base:#x}");
                map_elf(uspace, base, entry)
            })
            .transpose()?;

        let entry = VirtAddr::from_usize(
            ldso.as_ref()
                .map_or_else(|| elf.entry(), |ldso| ldso.entry()),
        );
        let auxv = elf
            .aux_vector(PAGE_SIZE_4K, ldso.map(|elf| elf.base()))
            .collect::<Vec<_>>();

        Ok((entry, auxv))
    }
}

static_lock! {
    static ELF_LOADER: Mutex<ElfLoader> = Mutex::new(ElfLoader::new());
}

/// Clear the ELF cache.
///
/// Useful for removing noise during memory leak detection.
pub fn clear_elf_cache() {
    ELF_LOADER.lock().0.flush();
    #[cfg(feature = "tee_ta_sign")]
    tee_task_iface::tasign::clear_ta_head_cache();
}

/// Load a user app from an owned executable request.
///
/// This is the exec-facing entry point when the caller has already resolved
/// the executable location, for example through a procfs magic link.
///
/// Requires task context, an initialized filesystem context for path/interpreter
/// lookup, and exclusive ownership of `uspace`. It may block on VFS and loader locks.
///
/// # Returns
///
/// Returns `(entry_point, initial_stack_pointer)` after mapping the executable,
/// optional interpreter, signal trampoline, stack, and heap.
///
/// # Errors
///
/// Returns `InvalidExecutable` for a non-ELF/non-script image, `FilesystemLoop`
/// after four script redirects, `InvalidInput` for invalid interpreter text or
/// interpreter image, and `ArgumentListTooLong` for an unrepresentable/oversized
/// initial stack. Propagates VFS, ELF parser (`InvalidData`), MM mapping/population/
/// write errors, and optional TA-signature rejection (`PermissionDenied`).
/// Failures after `MmSpace::clear` leave the old image destroyed; this API has
/// no rollback and the caller must not resume the previous user image.
///
/// # Panics
///
/// A prepared cache entry missing at commit violates an internal invariant and
/// panics. Current-context and filesystem initialization preconditions must
/// also hold. Invalid page offsets and short PT_INTERP reads return errors.
pub fn load_user_app_request(
    uspace: &mut MmSpace,
    request: ExecRequest,
) -> KResult<(VirtAddr, VirtAddr)> {
    load_user_app_request_inner(uspace, request, 0)
}

fn script_interpreter_args(line: &str, script_path: &str, original_args: &[String]) -> Vec<String> {
    line.trim()
        .splitn(2, |c: char| c.is_ascii_whitespace())
        .map(|s| s.trim_ascii().to_owned())
        .chain(iter::once(script_path.to_owned()))
        .chain(original_args.iter().skip(1).cloned())
        .collect()
}

fn load_user_app_request_inner(
    uspace: &mut MmSpace,
    request: ExecRequest,
    mut script_depth: usize,
) -> KResult<(VirtAddr, VirtAddr)> {
    let mut request = request;
    let prepared = loop {
        let binprm = request.prepare()?;
        match ELF_LOADER.lock().prepare_binprm(binprm)? {
            Ok(prepared) => break prepared,
            Err((binprm, data)) => {
                if !data.starts_with(b"#!") {
                    return Err(KError::InvalidExecutable);
                }
                if script_depth >= SCRIPT_RECURSION_MAX {
                    return Err(KError::FilesystemLoop);
                }
                let head = &data[2..data.len().min(256)];
                let pos = head
                    .iter()
                    .position(|byte| *byte == b'\n')
                    .unwrap_or(head.len());
                let line = core::str::from_utf8(&head[..pos]).map_err(|_| KError::InvalidInput)?;

                let new_args = script_interpreter_args(line, binprm.display_path(), binprm.args());
                let interpreter = new_args.first().ok_or(KError::InvalidInput)?.clone();
                request = ExecRequest::from_path(
                    interpreter,
                    new_args,
                    binprm.envs().to_vec(),
                    binprm.cred().clone(),
                );
                script_depth += 1;
            }
        }
    };

    let (entry, auxv) = ELF_LOADER
        .lock()
        .commit_prepared_binprm(uspace, &prepared)?;

    let ustack_top = VirtAddr::from_usize(kaddr_layout::USER_STACK_TOP);
    let ustack_size = kaddr_layout::USER_STACK_SIZE;
    let ustack_start = ustack_top - ustack_size;
    debug!("Mapping user stack: {ustack_start:#x?} -> {ustack_top:#x?}");

    uspace.map(
        ustack_start,
        ustack_size,
        MappingFlags::READ | MappingFlags::WRITE | MappingFlags::USER,
        false,
        VmRuntimeRef::new_anon_private(ustack_start, PageSize::Size4K),
    )?;

    let stack_data = app_stack_region(
        prepared.binprm.args(),
        prepared.binprm.envs(),
        &auxv,
        ustack_top.into(),
    )
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
        VmRuntimeRef::new_anon_private(heap_start, PageSize::Size4K),
    )?;

    Ok((entry, user_sp))
}

#[cfg(unittest)]
mod tests {
    use alloc::{borrow::ToOwned, vec};

    use khal::paging::{MappingFlags, PageSize};
    use memaddr::{MemoryAddr, VirtAddr, VirtAddrRange};
    use memspace::{VmArea, VmAreaSet, VmBackingInfo, VmBackingKind};
    use unittest::def_test;
    use xmas_elf::program::{FLAG_R, FLAG_W, FLAG_X, Flags};

    use super::{
        ExecRequest, ExecSource, LoadSegment, load_range_for_segments, mapping_flags,
        placement_reservation, script_interpreter_args,
    };

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

    #[def_test]
    fn exec_request_owns_path_args_and_envs() {
        let mut args = vec!["app".to_owned(), "one".to_owned()];
        let mut envs = vec!["A=B".to_owned()];
        let request = ExecRequest::from_path(
            "/bin/app",
            args.clone(),
            envs.clone(),
            kcred::initial_cred(),
        );

        args[0].push_str("-changed");
        envs[0].push_str("-changed");

        match request.source() {
            ExecSource::Path(path) => assert_eq!(path, "/bin/app"),
            ExecSource::Resolved { .. } => panic!("unexpected resolved source"),
        }
        assert_eq!(request.args().len(), 2);
        assert_eq!(request.args()[0], "app");
        assert_eq!(request.args()[1], "one");
        assert_eq!(request.envs().len(), 1);
        assert_eq!(request.envs()[0], "A=B");
    }

    #[def_test]
    fn script_interpreter_args_rewrites_linux_shape() {
        let original = vec![
            "/tmp/script.sh".to_owned(),
            "arg1".to_owned(),
            "arg2".to_owned(),
        ];
        let rewritten =
            script_interpreter_args("/bin/sh -e", "/tmp/script.sh", original.as_slice());

        assert_eq!(rewritten.len(), 5);
        assert_eq!(rewritten[0], "/bin/sh");
        assert_eq!(rewritten[1], "-e");
        assert_eq!(rewritten[2], "/tmp/script.sh");
        assert_eq!(rewritten[3], "arg1");
        assert_eq!(rewritten[4], "arg2");
    }

    fn load_segment(virtual_addr: usize, mem_size: usize) -> LoadSegment {
        LoadSegment {
            virtual_addr,
            mem_size,
            align: 0x1000,
        }
    }

    /// Independent copy of `map_elf`'s mapping arithmetic.
    ///
    /// Tests compare against this instead of hard-coded numbers: a literal
    /// expected value can be edited to match a wrong implementation, which is
    /// how the missing-page defect survived review once already.
    fn map_elf_pages(vaddr: usize, mem_size: usize) -> (usize, usize) {
        let mapped_start = VirtAddr::from_usize(vaddr).align_down_4k().as_usize();
        let mapped_size = VirtAddr::from_usize(vaddr.align_offset_4k() + mem_size)
            .align_up_4k()
            .as_usize();
        (mapped_start, mapped_start + mapped_size)
    }

    fn expected_range(segments: &[LoadSegment], base: usize) -> (usize, usize) {
        let mut start = usize::MAX;
        let mut end = 0;
        for segment in segments {
            let (segment_start, segment_end) =
                map_elf_pages(segment.virtual_addr + base, segment.mem_size);
            start = start.min(segment_start);
            end = end.max(segment_end);
        }
        (start, end)
    }

    #[def_test]
    fn load_range_covers_every_load_segment() {
        // Same shape as the shipped musl loader: an RX segment and a far RW one.
        let segments = [load_segment(0x0, 0xa19f4), load_segment(0xbfb00, 0x3410)];
        let base = 0x400_0000;
        let range = load_range_for_segments(&segments, base).expect("range");

        assert_eq!(
            (range.start.as_usize(), range.end.as_usize()),
            expected_range(&segments, base)
        );
        assert_eq!(range.size(), 0xc_3000);
        // The placement step reserves an `align`-aligned span, because
        // `find_free_area` reports an aligned start rather than an aligned end.
        assert_eq!(
            VirtAddr::from_usize(range.size())
                .align_up(0x1_0000usize)
                .as_usize(),
            0xd_0000
        );
    }

    #[def_test]
    fn load_range_adds_the_page_offset_before_rounding() {
        // A segment that starts mid-page and does not end on a page boundary
        // needs a partial final page. Rounding `align_down(vaddr) + mem_size`
        // instead of `align_offset(vaddr) + mem_size` drops that page, so the
        // placement search would treat a mapped page as free.
        let segments = [load_segment(0x10dd0, 0x5000298)];
        let base = 0x1000;
        let range = load_range_for_segments(&segments, base).expect("range");

        assert_eq!(range.end.as_usize(), 0x501_3000);
        assert_eq!(range.size(), 0x500_2000);
        assert_eq!(range.end.as_usize(), expected_range(&segments, base).1);
    }

    #[def_test]
    fn load_range_agrees_with_map_elf_for_many_layouts() {
        for page_offset in [0usize, 1, 0x40, 0x800, 0xfff] {
            for mem_size in [1usize, 0x1000, 0x1fff, 0x2000, 0x3410, 0xa19f4, 0x5000298] {
                let segments = [
                    load_segment(page_offset, mem_size),
                    load_segment(0xbfb00, 0x3410),
                ];
                let base = 0x400_0000;
                let range = load_range_for_segments(&segments, base).expect("range");

                assert_eq!(
                    (range.start.as_usize(), range.end.as_usize()),
                    expected_range(&segments, base),
                    "vaddr={page_offset:#x} mem_size={mem_size:#x}"
                );
            }
        }
    }

    #[def_test]
    fn load_range_includes_page_rounding_of_large_bss() {
        // An 80 MiB BSS in a PIE whose RW segment starts mid-page: the range
        // must reach past the interpreter hint so placement can detect it.
        let segments = [load_segment(0x0, 0x974), load_segment(0x10dd0, 0x5000298)];
        let base = 0x1000;
        let range = load_range_for_segments(&segments, base).expect("range");

        assert_eq!(
            (range.start.as_usize(), range.end.as_usize()),
            expected_range(&segments, base)
        );
        assert_eq!(range.end.as_usize(), 0x501_3000);
        // The span must cover the fixed interpreter hint that the old loader
        // used unconditionally.
        assert!(range.contains(VirtAddr::from_usize(0x400_0000)));
    }

    /// Exercise the same VMA search used by MmSpace, with one free page
    /// immediately before an occupied page. A search-algorithm copy would not
    /// protect this contract when memspace changes.
    #[def_test]
    fn placement_reserves_the_region_it_verifies() {
        let base = 0x0400_0000usize;
        let align = 0x1000usize;
        let hint = VirtAddr::from_usize(base);
        let limit = VirtAddrRange::new(hint, VirtAddr::from_usize(base + 0x1_0000));
        let segments = [load_segment(0x1000, 0x1000)];
        let span = load_range_for_segments(&segments, base).expect("span");
        let mut occupied = VmAreaSet::new();
        let flags = MappingFlags::USER | MappingFlags::READ;
        for (start, size) in [(0x1000, base - 0x1000), (base + 0x1000, 0x1000)] {
            occupied
                .try_insert(VmArea::new(
                    VirtAddr::from_usize(start),
                    size,
                    flags,
                    flags,
                    VmBackingInfo::new(VmBackingKind::Linear, PageSize::Size4K),
                    0,
                    None,
                ))
                .expect("non-overlapping main image");
        }

        // The old reservation accepts the hole, but the interpreter's first
        // mapped page then lands on the main image. This is the negative control.
        let old_bias = occupied
            .find_free_area(hint, span.size(), limit, align)
            .expect("old reservation finds the hole");
        assert_eq!(old_bias, hint);
        let old_mapping =
            load_range_for_segments(&segments, old_bias.as_usize()).expect("old mapping");
        assert!(occupied.overlaps(old_mapping));

        let size = placement_reservation(span, base, align).expect("reservation");
        assert_eq!(size, 0x2000);
        let bias = occupied
            .find_free_area(hint, size, limit, align)
            .expect("space after main image");
        assert_eq!(bias.as_usize(), base + 0x2000);
        let mapped = load_range_for_segments(&segments, bias.as_usize()).expect("mapping");
        assert!(!occupied.overlaps(mapped));
        assert!(bias <= mapped.start && mapped.end.as_usize() <= bias.as_usize() + size);
    }

    #[def_test]
    fn load_range_rejects_images_without_load_segments() {
        assert!(load_range_for_segments(&[], 0x400_0000).is_none());
    }

    #[def_test]
    fn load_range_rejects_overflowing_segment_end() {
        let segments = [load_segment(0x0, usize::MAX)];

        assert!(load_range_for_segments(&segments, 0x400_0000).is_none());
    }
}
