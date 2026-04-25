use allocator_api2::{boxed::Box, vec::Vec};
use allocator_memfd_secret as fdsec;
use core::mem::MaybeUninit;

#[test]
fn works() {
    let Ok(file) = fdsec::SecretArena::new() else {
        // Can't test..
        panic!("Untestable system")
    };

    let alloc_hdl = &file;
    let value = Box::new_in(5, alloc_hdl);
    assert_eq!(*value, 5);

    let slice = Box::<[MaybeUninit<u8>], _>::new_zeroed_slice_in(1024, alloc_hdl);
    assert_eq!(slice.len(), 1024);
}

#[test]
fn expand() {
    let Ok(file) = fdsec::SecretArena::new() else {
        // Can't test..
        panic!("Untestable system")
    };

    let alloc_hdl = &file;
    let mut elements = Vec::new_in(alloc_hdl);

    for _ in 0..1024 {
        let slice = Box::<[MaybeUninit<u8>], _>::new_zeroed_slice_in(1024, alloc_hdl);
        elements.push(slice);
    }
}

#[test]
fn munmap() {
    let Ok(file) = fdsec::SecretArena::new() else {
        // Can't test..
        panic!("Untestable system")
    };

    {
        assert!(file.mapped_memory() == 0);
        let _value = Box::new_in(5, &file);

        // With a value allocated we have mapped pages and used some memory.
        assert!(file.mapped_memory() > 0);
        assert!(file.mem_used() > 0);
    }

    // With the value dropped, we're no longer 'using' anything but the pages still reside in
    // memory. So they are unused in terms of the user API but used in terms of the OS.
    assert!(file.mapped_memory() > 0);
    assert!(file.mem_used() == 0);
    assert!(file.mem_free() > 0);

    // After unmapping, the state is back to a clean slate. No more pages used.
    let file = fdsec::SecretArena::unmap(file);
    assert!(file.mapped_memory() == 0);
    assert!(file.mem_free() == 0);
    assert!(file.mem_used() == 0);
}

#[test]
fn no_leaks() {
    let Ok(mut file) = fdsec::SecretArena::new() else {
        // Can't test..
        panic!("Untestable system")
    };

    // Check that we do not leak by repeatedly allocating and resetting.
    for _ in 0..(1 << 16) {
        assert!(file.mapped_memory() == 0);
        // Allocate (and thus map) something from the file, then drop and dealloc again.
        let _ = Box::<[MaybeUninit<u8>], _>::new_uninit_slice_in(1 << 12, &file);
        // Reset for the next loop.
        file = fdsec::SecretArena::unmap(file);
    }

    assert!(file.mapped_memory() == 0);
    assert!(file.mem_free() == 0);
    assert!(file.mem_used() == 0);
}

#[test]
fn graceful_limits() {
    let rlimits = {
        let Ok(file) = fdsec::SecretArena::new() else {
            // Can't test..
            panic!("Untestable system")
        };

        // Since we default to the soft-limit, this should be the soft limit.
        file.memfd_len()
    };

    let Ok(file) = fdsec::SecretArena::with_size_limit(rlimits.saturating_mul(1024)) else {
        // Can't test..
        panic!("Untestable system")
    };

    let mut v = Vec::<u8, _>::new_in(&file);
    let mut count_to_error = 7..32;

    let error_at = loop {
        let Some(i) = count_to_error.next() else {
            // Weird system configuration, we could not provoke that error..
            return;
        };

        if v.try_reserve(1usize << i).is_err() {
            break i;
        };
    };

    // We provoked running out of pages that count to the MLOCK limit. The allocator should still
    // work fine by itself.
    assert!(file.failed_mmap());
    assert!(file.mapped_memory() > 0);
    assert!(file.mem_used() < (1usize << error_at));
    assert!(file.mem_used() >= (1usize << error_at - 1));

    // Let's return and test if the allocator still works in-place.
    drop(v);
    assert!(file.mapped_memory() > 0);
    assert_eq!(file.mem_used(), 0);

    {
        let _boxed = Box::<[MaybeUninit<u8>], _>::new_uninit_slice_in(1 << 10, &file);
        assert!(file.mapped_memory() > 0);
        assert!(file.mem_used() > 0);
    }

    // And of course we can always unmap.
    let file = fdsec::SecretArena::unmap(file);
    assert!(!file.failed_mmap());
    assert_eq!(file.mapped_memory(), 0);
}
