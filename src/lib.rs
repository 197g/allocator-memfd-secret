#![no_std]

use allocator_api2::alloc::{AllocError, Allocator, Layout};
use core::{cell, ptr};

/// An arena allocator, backed by a memfd-secret.
pub struct SecretArena {
    fd: libc::c_int,
    maps: [cell::Cell<ptr::NonNull<[u8]>>; 48],
    size_pot: cell::Cell<u8>,
    fns: FnTable,
}

const MIN_SZ_POT2: u8 = 12;

impl SecretArena {
    pub fn new() -> Result<Self, ()> {
        let res = unsafe { libc::syscall(libc::SYS_memfd_secret, libc::FD_CLOEXEC) };

        if res < 0 {
            return Err(());
        }

        Ok(SecretArena {
            fd: res as libc::c_int,
            maps: [const { cell::Cell::new(ptr::NonNull::from_mut(&mut [])) }; 48],
            size_pot: cell::Cell::new(0),
            fns: FnTable::libc(),
        })
    }

    fn pre_ensure_size(&self, target: Layout) {
        if target.size() == 0 {
            return;
        }

        let pot = 0usize.leading_zeros() - target.pad_to_align().size().leading_zeros();
        let pot = pot.saturating_sub(u32::from(MIN_SZ_POT2));

        // Will fail to allocate later, okay.
        if pot > self.maps.len() as u32 {
            return;
        }

        for (newpot, map) in self.maps[usize::from(self.size_pot.get())..=pot as usize]
            .iter()
            .enumerate()
            .map(|(idx, mapping)| (idx as u32 + u32::from(MIN_SZ_POT2), mapping))
        {
            let (offset, len) = Self::offset_and_len_for(newpot);

            if 0 > unsafe { (self.fns.truncate)(self.fd, (offset + len) as i64) } {
                return;
            }

            let mapping = unsafe { (self.fns.mmap_at)(self.fd, len, offset as i64) };

            let Some(ptr) = ptr::NonNull::new(mapping) else {
                return;
            };

            let memory = ptr::slice_from_raw_parts_mut(ptr.as_ptr().cast(), len);
            map.set(ptr::NonNull::new(memory).unwrap());

            self.size_pot.update(|n| n + 1);
        }
    }

    fn offset_and_len_for(pot: u32) -> (usize, usize) {
        debug_assert!(pot < 48);
        debug_assert!(pot >= MIN_SZ_POT2 as u32);

        let bitmask = |n: u32| -> usize {
            assert!(n < 0usize.leading_zeros());
            ((1u32 << n) - 1) as usize
        };

        let mapped_len = bitmask(pot) + 1;
        let total = bitmask(pot) & !bitmask(u32::from(MIN_SZ_POT2));

        (total - mapped_len, mapped_len)
    }
}

impl Drop for SecretArena {
    fn drop(&mut self) {
        for (idx, ptr) in self.maps[..usize::from(self.size_pot.get())]
            .iter()
            .enumerate()
        {
            let len = 1usize << (idx + usize::from(MIN_SZ_POT2));
            unsafe { (self.fns.munmap)(ptr.as_ptr().cast::<libc::c_void>(), len) };
        }

        unsafe { (self.fns.close)(self.fd) };
    }
}

unsafe impl Allocator for SecretArena {
    fn allocate(&self, layout: Layout) -> Result<ptr::NonNull<[u8]>, AllocError> {
        self.pre_ensure_size(layout);
        todo!()
    }

    unsafe fn deallocate(&self, ptr: ptr::NonNull<u8>, layout: Layout) {
        todo!()
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
                    libc::MAP_PRIVATE,
                    fd,
                    offset,
                )
            },
            truncate: |fd, len| unsafe { libc::ftruncate64(fd, len) },
            munmap: |ptr, len| unsafe { libc::munmap(ptr, len) },
            close: |fd| unsafe { libc::close(fd) },
        }
    }
}
