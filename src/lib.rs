// #![no_std]

use allocator_api2::alloc::{AllocError, Allocator, Layout};
use core::{alloc::GlobalAlloc, ptr};

use linked_list_allocator::LockedHeap;
use spin::Once;

/// An arena allocator, backed by a memfd-secret.
pub struct SecretArena {
    fd: libc::c_int,
    maps: [Once<Option<Mapping>>; 48],
    len_limit: usize,
    truncates: Once<bool>,
    fns: FnTable,
}

struct Mapping {
    // If we fail to establish a mapping, that is terminal.
    inner: ptr::NonNull<[u8]>,
    heap: LockedHeap,
}

const MIN_SZ_POT2: u8 = 12;

#[derive(Clone, Copy)]
struct MapIndexForPot(u8);

impl SecretArena {
    pub fn new() -> Result<Self, ()> {
        let res = unsafe { libc::syscall(libc::SYS_memfd_secret, 0) };

        if 0 > res {
            return Err(());
        }

        let mut limits: libc::rlimit = libc::rlimit {
            rlim_cur: 0,
            rlim_max: 0,
        };

        if 0 > unsafe { libc::getrlimit(libc::RLIMIT_MEMLOCK, &mut limits) } {
            return Err(());
        }

        Ok(SecretArena {
            fd: res as libc::c_int,
            maps: [const { Once::new() }; 48],
            len_limit: limits.rlim_cur as usize,
            truncates: Once::new(),
            fns: FnTable::libc(),
        })
    }

    /// The length of the underlying file.
    ///
    /// Note that page allocation is lazily performed with mmap or after faulting in pages that
    /// have been mmaped. This does not reflect the *current* usage of the MLOCK budget, only the
    /// potential usage.
    pub fn memfd_len(&self) -> usize {
        self.len_limit
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
            eprintln!("Failed to truncate {} {}", offset + len, unsafe {
                libc::sysconf(libc::_SC_PAGESIZE)
            });

            return false;
        }

        // We should hit this only if you passed a customized function table.
        #[cold]
        fn mmap_null_ptr_oopsie<T>() -> Option<ptr::NonNull<T>> {
            panic!("Bad mmap: {}", unsafe { *libc::__errno_location() });
        }

        let mapping = map.call_once(|| {
            let ptr = unsafe { (self.fns.mmap_at)(self.fd, len, offset as i64) };

            if ptr == libc::MAP_FAILED {
                eprintln!("Mmap failed {len}, {offset}");
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

        if mapping.is_none() {
            eprintln!("Mapping {newpot} not established");
            return false;
        };

        true
    }
}

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

unsafe impl Allocator for SecretArena {
    fn allocate(&self, layout: Layout) -> Result<ptr::NonNull<[u8]>, AllocError> {
        if layout.size() == 0 {
            // MSRV: 1.95. But nice feature that avoids a badly aligned accident.
            let dangling = layout.dangling_ptr();
            return Ok(ptr::NonNull::slice_from_raw_parts(dangling, 0));
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
                return Err(AllocError);
            };

            // Safety: checked zero-size above.
            let allocation = unsafe { map.heap.alloc(layout) };

            if let Some(base) = ptr::NonNull::new(allocation) {
                let len = layout.size();

                debug_assert!({
                    let byte_offset = base.as_ptr().addr().wrapping_sub(map.inner.as_ptr().addr());
                    byte_offset < map.inner.len()
                });

                return Ok(ptr::NonNull::slice_from_raw_parts(base, len));
            }
        }

        Err(AllocError)
    }

    unsafe fn deallocate(&self, ptr: ptr::NonNull<u8>, layout: Layout) {
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

            let byte_offset = ptr.as_ptr().addr().wrapping_sub(map.inner.as_ptr().addr());

            // Can't have zero-length allocations, anything on the boundary belongs to the another
            // map if any.
            if byte_offset >= map.inner.len() {
                continue;
            }

            unsafe { map.heap.dealloc(ptr.as_ptr(), layout) };
            return;
        }

        debug_assert!(false, "Tried to deallocate a pointer from a wrong heap");
        // Do nothing without debug assertions, leaks the pointer but is internally sound.
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
