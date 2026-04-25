#![no_std]
#![cfg_attr(
    feature = "allocator-api2",
    doc = include_str!("../Readme.md")
)]

#[cfg(feature = "allocator-api2")]
use allocator_api2::alloc::{AllocError, Allocator};

use core::{
    alloc::{GlobalAlloc, Layout},
    ptr,
};

#[cfg(target_has_atomic = "ptr")]
use core::sync::atomic::{AtomicUsize, Ordering};

use linked_list_allocator::LockedHeap;
use spin::Once;

/// An arena allocator, backed by a memfd-secret.
///
/// This value is quite large by design. That should not be an issue if put into a static as a
/// global allocator, however in other situations you may prefer boxing it up itself. For the
/// former you have to bring your own initialization routine (that copes with or ignores
/// allocations requested before the file descriptor is fully opened).
///
/// # Usage
///
/// Use this for raw (pointer) allocations that you manipulate carefully and only pass to:
///
/// - functions written in assembly.
/// - functions hardened against leakage by assembly checks.
/// - OS interfaces that you have verified not to create shadow caches of it anywhere and do the
///   same, best if they do not deal with the contents at all.
///
/// Please do __not__ use a `Vec` from this allocator and then pass it to `Read::read_to_end`. That
/// subverts the main point as the contents may end up in registers or temporary buffers etc.
/// Instead,  Well, I'm not the cyber-police so you may do it anyways and the result is certainly
/// better than not caring at all—but still not *good*.
///
/// Please note that any live value will keep your OS from hibernating, like any locked pages
/// would. If you care about battery lifetime then use this sparingly, temporarily, and clean this
/// up. Unlike `mlock(2)`, this effect is scoped to the file descriptor according to the
/// documentation, not merely existing mappings of the pages.
pub struct SecretArena {
    /// The functions used to interact with the OS.
    fns: FnTable,
    fd: libc::c_int,
    len_limit: usize,
    maps: [Once<Option<Mapping>>; 48],
    truncates: Once<bool>,
    diagnostics: Diagnostics,
}

struct Mapping {
    // If we fail to establish a mapping, that is terminal.
    inner: ptr::NonNull<[u8]>,
    heap: LockedHeap,
}

// Motivated by one typical PAGE_SZ.
//
// FIXME: `mmap` will probably fail on systems where we only have larger pages than this. So this
// should be a runtime constant? Then we have some entries in `maps` that are pointless but that's
// fine? Or do we create a starting bias instead and keep the start of that array untouched? Maybe
// provide a PR motivated by such a system if not fixed yet.
const MIN_SZ_POT2: u8 = 12;

#[derive(Clone, Copy)]
struct MapIndexForPot(u8);

impl SecretArena {
    pub fn new() -> Result<Self, MemfdSecretFailed> {
        let mut limits: libc::rlimit = libc::rlimit {
            rlim_cur: 0,
            rlim_max: 0,
        };

        // Safety: Valid resource for the call. Signature:
        //
        //     int getrlimit(int resource, struct rlimit *rlp);
        //
        // Though this is Linux specific, on other systems `libc` does not expose it. At worst I
        // expect that to be rejected with an error code.
        if 0 > unsafe { libc::getrlimit(libc::RLIMIT_MEMLOCK, &mut limits) } {
            return Err(MemfdSecretFailed::INIT);
        }

        let size = usize::try_from(limits.rlim_cur).unwrap_or(usize::MAX);
        Self::with_size_limit(size)
    }

    /// Create a `memfd_secret` with a pre-determined size.
    ///
    /// Using a size larger than the `RLIMIT_MEMLOCK` limits of the process (see `mlock(2)`) will
    /// appear to work but may fail at runtime when mapping pages from the file to the process.
    pub fn with_size_limit(size: usize) -> Result<Self, MemfdSecretFailed> {
        // Safety: `0` is a valid flag for this syscall. Signature:
        //
        //     int syscall(SYS_memfd_secret, unsigned int flags);.
        let res = unsafe { libc::syscall(libc::SYS_memfd_secret, 0) };

        if 0 > res {
            return Err(MemfdSecretFailed::INIT);
        }

        Ok(SecretArena {
            fd: res as libc::c_int,
            maps: [const { Once::new() }; 48],
            len_limit: size,
            truncates: Once::new(),
            fns: FnTable::libc(),
            diagnostics: Default::default(),
        })
    }

    /// Unmap all pages of the file, resetting the allocator.
    ///
    /// You should assume that this drops the value passed in, implying also that any allocations
    /// that were allocated from it are no longer considered live after this. Note that the file
    /// size, [`Self::memfd_len`], will be unaffected and can not be changed.
    ///
    /// This unlocks all pages that were mapped into the process.
    ///
    /// Design note: Taking `&mut self` would place somewhat underspecified requirements on this
    /// method with the [`Allocator`][`allocator_api2::alloc::Allocator`] safety contract. It's not
    /// clear if the value can be considered the same allocator afterwards, which would not allow
    /// us to invalidate existing allocations. The value is not `Clone` and we _could_ have
    /// replaced it with another in this method but it's unclear what identity is used here.
    pub fn unmap(mut this: Self) -> Self {
        for map in &mut this.maps {
            if let Some(Some(mapping)) = map.get_mut() {
                let addr = mapping.inner.as_ptr().cast::<libc::c_void>();
                let len = mapping.inner.len();
                unsafe { (this.fns.munmap)(addr, len) };
            }

            *map = Once::new();
        }

        this.diagnostics.reset();

        this
    }

    /// The length of the underlying file.
    ///
    /// Note that page allocation is lazily performed with mmap or after faulting in pages that
    /// have been mmaped. This does not reflect the *current* usage of the MLOCK budget, only the
    /// potential usage.
    pub fn memfd_len(&self) -> usize {
        self.len_limit
    }

    /// Query the number of mapped bytes from the file.
    ///
    /// This is an approximation as it may be out-of-date by the time the value is processed, if
    /// other concurrent use of the allocator incurs additional mappings. It will never exceed
    /// [`Self::memfd_len`].
    ///
    /// On systems that do not provide pointer-sized atomics (unlikely to exist, but technically
    /// possible) this will always return `0`.
    pub fn mapped_memory(&self) -> usize {
        self.diagnostics.get_mapped_memory()
    }

    /// Did a `truncate` syscall fail?
    ///
    /// That is not fatal but essentially leaves this allocator without any resources. That is,
    /// only zero-sized allocations can be served.
    ///
    /// On systems that do not provide pointer-sized atomics (unlikely to exist, but technically
    /// possible) this will always return `false`.
    pub fn failed_truncate(&self) -> bool {
        self.diagnostics.get_failure_value() & Diagnostics::MASK_TRUNCATE != 0
    }

    /// Did an `mmap` syscall fail?
    ///
    /// That is not fatal but leaves this allocator with limited resources. The total amount of
    /// memory that can be served is fixed after this point. The failure may indicate that the
    /// number of pages would exceed the allowed limit of `RLIMIT_MEMLOCK` pages.
    ///
    /// On systems that do not provide pointer-sized atomics (unlikely to exist, but technically
    /// possible) this will always return `false`.
    pub fn failed_mmap(&self) -> bool {
        self.diagnostics.get_failure_value() & Diagnostics::MASK_MMAP != 0
    }

    /// Query the amount of memory used.
    pub fn mem_used(&self) -> usize {
        let mut total = 0;

        for map in &self.maps {
            let Some(Some(map)) = map.get() else {
                break;
            };

            if let Some(guard) = map.heap.try_lock() {
                total += guard.used();
            }
        }

        total
    }

    /// Query the amount of memory free.
    ///
    /// This only includes memory from the file descriptor that was already mapped. It is generally
    /// smaller than [`Self::mapped_memory`] except there is no guaranteed synchronization between
    /// the two.
    pub fn mem_free(&self) -> usize {
        let mut total = 0;

        for map in &self.maps {
            let Some(Some(map)) = map.get() else {
                break;
            };

            if let Some(guard) = map.heap.try_lock() {
                total += guard.free();
            }
        }

        total
    }

    fn pre_ensure_size(&self, target: Layout) {
        if target.size() == 0 {
            return;
        }

        let pot = Self::to_minimum_map_index(target.pad_to_align().size());
        // Will fail to allocate later, okay.
        if usize::from(pot.0) > self.maps.len() {
            return;
        }

        for newpot in 0..=pot.0 as usize {
            // Safety: Due to the loop iteration, all previous iterations returned `true`.
            if !unsafe { self.expand_mapping(MapIndexForPot(newpot as u8)) } {
                return;
            }
        }
    }

    fn offset_and_len_for(MapIndexForPot(pot): MapIndexForPot) -> (usize, usize) {
        debug_assert!(pot < 48);
        let pot = pot + MIN_SZ_POT2;

        let bitmask = |n: u8| -> usize {
            assert!(u32::from(n) < 0usize.leading_zeros());
            ((1u32 << n) - 1) as usize
        };

        let mapped_len = bitmask(pot) + 1;
        let total = bitmask(pot + 1) & !bitmask(MIN_SZ_POT2);

        (total - mapped_len, mapped_len)
    }

    fn to_minimum_map_index(size: usize) -> MapIndexForPot {
        debug_assert!(size <= isize::MAX as usize, "Invalid allocation size");
        let pot = 0usize.leading_zeros() - size.leading_zeros();
        MapIndexForPot(pot.saturating_sub(u32::from(MIN_SZ_POT2)) as u8)
    }

    /// # Safety
    ///
    /// Must have been successfully called for all smaller indices previously. Guarantees that the
    /// mapping is not initialized or `None` until this returned `true`. This is required to ensure
    /// the file only _grows_ while this heap is alive, otherwise previous mmap together with
    /// anything allocated in them might get invalidated.
    unsafe fn expand_mapping(&self, idx @ MapIndexForPot(newpot): MapIndexForPot) -> bool {
        let (offset, len) = Self::offset_and_len_for(idx);

        let map = &self.maps[usize::from(newpot)];

        if offset + len > self.len_limit {
            return false;
        }

        let successfully_truncated_at_least = self
            .truncates
            .call_once(|| 0 == unsafe { (self.fns.truncate)(self.fd, self.len_limit as i64) });

        if !successfully_truncated_at_least {
            self.diagnostics.truncate_failed(self.len_limit);
            return false;
        }

        // We should hit this only if you passed a customized function table.
        #[cold]
        fn mmap_null_ptr_oopsie<T>() -> Option<ptr::NonNull<T>> {
            panic!("Bad mmap implementation: {}", unsafe {
                *libc::__errno_location()
            });
        }

        let mut called_here = false;
        let mapping = map.call_once(|| {
            called_here = true;

            let ptr = unsafe { (self.fns.mmap_at)(self.fd, len, offset as i64) };

            if ptr == libc::MAP_FAILED {
                return None;
            }

            let base = ptr::NonNull::new(ptr)
                .or_else(mmap_null_ptr_oopsie::<_>)?
                .cast::<u8>();

            // SAFETY: valid, exclusive r/w mapping. The docs say must be `'static` but
            // that is somewhat untrue. Works with any other lifetime as well with the
            // caveat that allocations are also constrained (i.e. we must not give out the
            // internal heap as an allocator handle, just constrained reference to it).
            //
            // Chances are also you're using this allocator as a global one where this _is_
            // actually static.
            let heap = unsafe { LockedHeap::new(base.as_ptr(), len) };

            Some(Mapping {
                heap,
                inner: ptr::NonNull::slice_from_raw_parts(base, len),
            })
        });

        if called_here && mapping.is_none() {
            self.diagnostics.mmap_failed();
        } else if called_here {
            self.diagnostics.mapped_memory(len);
        }

        if mapping.is_none() {
            return false;
        };

        true
    }

    fn alloc_optional(&self, layout: Layout) -> Option<ptr::NonNull<[u8]>> {
        if layout.size() == 0 {
            // MSRV: 1.95. But nice feature that avoids a badly aligned accident.
            let dangling = layout.dangling_ptr();
            return Some(ptr::NonNull::slice_from_raw_parts(dangling, 0));
        }

        self.pre_ensure_size(layout);

        let first_try = Self::to_minimum_map_index(layout.pad_to_align().size());

        for map_idx in usize::from(first_try.0)..48 {
            if map_idx != usize::from(first_try.0) {
                // SAFETY: at least one previous iterations, which had a valid map certifying it
                // was successfully initialized. Visits all maps in order.
                if !unsafe { self.expand_mapping(MapIndexForPot(map_idx as u8)) } {
                    break;
                }
            }

            let Some(Some(map)) = self.maps[map_idx].get() else {
                return None;
            };

            // Safety: checked zero-size above.
            let allocation = unsafe { map.heap.alloc(layout) };

            if let Some(base) = ptr::NonNull::new(allocation) {
                let len = layout.size();

                debug_assert!({
                    let byte_offset = base.as_ptr().addr().wrapping_sub(map.inner.as_ptr().addr());
                    byte_offset < map.inner.len()
                });

                return Some(ptr::NonNull::slice_from_raw_parts(base, len));
            }
        }

        None
    }
}

// Sufficient but minimal impls that make the allocator `Send + Sync`.
//
// Safety: The pointers/mmap we keep are valid for the whole process. Sending implies ownership of
// the allocator, nothing can be invalidated.
unsafe impl Send for Mapping {}

// Safety: we do not provide access to any resource that is not `Sync` itself. The mapping is never
// dereferenced, the allocator is itself `Sync` and only accessed by shared reference.
unsafe impl Sync for Mapping {}

const _IS_SEND_AND_SYNC: () = {
    const fn requires_send_and_sync<T: Send + Sync + 'static>() {}

    requires_send_and_sync::<SecretArena>();
};

impl Drop for SecretArena {
    fn drop(&mut self) {
        for map in &mut self.maps {
            if let Some(Some(mapping)) = map.get_mut() {
                let addr = mapping.inner.as_ptr().cast::<libc::c_void>();
                let len = mapping.inner.len();
                unsafe { (self.fns.munmap)(addr, len) };
            }
        }

        unsafe { (self.fns.close)(self.fd) };
    }
}

unsafe impl GlobalAlloc for SecretArena {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        self.alloc_optional(layout)
            .map_or_else(ptr::null_mut, |ptr| ptr.as_ptr().cast())
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        if layout.size() == 0 {
            // Nothing to do, this was allocated as dangling.
            return;
        }

        let first_try = Self::to_minimum_map_index(layout.pad_to_align().size());

        // Figure out which map it was allocated from.
        for map in &self.maps[usize::from(first_try.0)..] {
            let Some(Some(map)) = map.get() else {
                continue;
            };

            let byte_offset = ptr.addr().wrapping_sub(map.inner.as_ptr().addr());

            // Can't have zero-length allocations, anything on the boundary belongs to the another
            // map if any.
            if byte_offset >= map.inner.len() {
                continue;
            }

            unsafe { map.heap.dealloc(ptr, layout) };
            return;
        }

        debug_assert!(false, "Tried to deallocate a pointer from a wrong heap");
        // Do nothing without debug assertions, leaks the pointer but is internally sound.
    }
}

#[cfg(feature = "allocator-api2")]
unsafe impl Allocator for SecretArena {
    fn allocate(&self, layout: Layout) -> Result<ptr::NonNull<[u8]>, AllocError> {
        self.alloc_optional(layout).ok_or(AllocError)
    }

    unsafe fn deallocate(&self, ptr: ptr::NonNull<u8>, layout: Layout) {
        // Safety:
        // - a block of this allocator, as ensured by `allocator-api2`.
        // - the only layout that 'fits' is the one used precisely so `layout` must be the one that
        //   was used and we can forward that requirement from `allocator-api2`.
        unsafe { GlobalAlloc::dealloc(self, ptr.as_ptr(), layout) }
    }
}

#[derive(Debug)]
// Force the compiler to make some decisions as if this could have all potential obstacles, do not
// make structural inference about this type as much as possible.
#[non_exhaustive]
pub struct MemfdSecretFailed {
    // Private internals, and not a zero-sized struct.
    #[allow(dead_code)]
    inner: u8,
}

impl MemfdSecretFailed {
    const INIT: Self = MemfdSecretFailed { inner: 0 };
}

impl core::fmt::Display for MemfdSecretFailed {
    fn fmt(&self, f: &mut core::fmt::Formatter) -> core::fmt::Result {
        f.write_str("failed to instantiate a `memfd_secret`")
    }
}

#[derive(Default)]
struct Diagnostics {
    #[cfg(target_has_atomic = "ptr")]
    failure: AtomicUsize,
    #[cfg(target_has_atomic = "ptr")]
    mapped_memory: AtomicUsize,
}

impl Diagnostics {
    const MASK_TRUNCATE: usize = 1 << 0;
    const MASK_MMAP: usize = 1 << 1;

    fn reset(&mut self) {
        #[cfg(target_has_atomic = "ptr")]
        {
            *self.failure.get_mut() = 0;
            *self.mapped_memory.get_mut() = 0;
        }
    }

    fn mapped_memory(&self, len: usize) {
        self.mapped_memory.fetch_add(len, Ordering::Relaxed);
    }

    fn truncate_failed(&self, _requested_len: usize) {
        #[cfg(target_has_atomic = "ptr")]
        self.failure
            .fetch_or(Self::MASK_TRUNCATE, Ordering::Relaxed);
    }

    fn mmap_failed(&self) {
        #[cfg(target_has_atomic = "ptr")]
        self.failure.fetch_or(Self::MASK_MMAP, Ordering::Relaxed);
    }

    fn get_mapped_memory(&self) -> usize {
        #[cfg(target_has_atomic = "ptr")]
        {
            self.mapped_memory.load(Ordering::Relaxed)
        }

        #[cfg(not(target_has_atomic = "ptr"))]
        {
            0
        }
    }

    fn get_failure_value(&self) -> usize {
        #[cfg(target_has_atomic = "ptr")]
        {
            self.failure.load(Ordering::Relaxed)
        }

        #[cfg(not(target_has_atomic = "ptr"))]
        {
            0
        }
    }
}

struct FnTable {
    mmap_at: unsafe fn(libc::c_int, len: usize, at: i64) -> *mut libc::c_void,
    truncate: unsafe fn(libc::c_int, len: i64) -> libc::c_int,
    munmap: unsafe fn(*mut libc::c_void, len: usize) -> libc::c_int,
    close: unsafe fn(libc::c_int) -> libc::c_int,
}

impl FnTable {
    fn libc() -> Self {
        FnTable {
            mmap_at: |fd, len, offset| unsafe {
                libc::mmap64(
                    ptr::null_mut(),
                    len,
                    libc::PROT_READ | libc::PROT_WRITE,
                    // Note that `PRIVATE` will not work.
                    libc::MAP_SHARED | libc::MAP_LOCKED,
                    fd,
                    offset,
                )
            },
            truncate: |fd, len| unsafe { libc::ftruncate(fd, len) },
            munmap: |ptr, len| unsafe { libc::munmap(ptr, len) },
            close: |fd| unsafe { libc::close(fd) },
        }
    }
}
