use core::mem::MaybeUninit;
use allocator_memfd_secret as fdsec;
use allocator_api2::{boxed::Box, vec::Vec};

#[test]
fn works() {
    let Ok(file) = fdsec::SecretArena::new() else {
        // Can't test..
        panic!("Incorrect system")
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
        panic!("Incorrect system")
    };

    let alloc_hdl = &file;
    let mut elements = Vec::new_in(alloc_hdl);

    for _ in 0..1024 {
        let slice = Box::<[MaybeUninit<u8>], _>::new_zeroed_slice_in(1024, alloc_hdl);
        elements.push(slice);
    }
}
