use allocator_api2::vec::Vec;
use allocator_memfd_secret as fdsec;

// Check that `unmap` actually relieves pressure.
#[test]
fn unmap_returns_concurrent_resources() {
    let Ok(file) = fdsec::SecretArena::new() else {
        // Can't test..
        panic!("Untestable system")
    };

    let primary_limit = {
        let mut v = Vec::<u8, _>::new_in(&file);
        let mut count_to_error = 7..32;

        loop {
            let Some(i) = count_to_error.next() else {
                // Weird system configuration, we could not provoke that error..
                return;
            };

            if v.try_reserve(1usize << i).is_err() {
                break i;
            };
        }
    };

    let Ok(secondary) = fdsec::SecretArena::new() else {
        // Can't test..
        panic!("Untestable system")
    };

    let secondary_limit = {
        let mut v = Vec::<u8, _>::new_in(&secondary);
        let mut count_to_error = 7..32;

        loop {
            let Some(i) = count_to_error.next() else {
                // Weird system configuration, we could not provoke that error..
                return;
            };

            if v.try_reserve(1usize << i).is_err() {
                break i;
            };
        }
    };

    assert!(primary_limit > secondary_limit, "{primary_limit}/{secondary_limit}");
    let file = fdsec::SecretArena::unmap(file);
    let secondary = fdsec::SecretArena::unmap(secondary);

    let mut v = Vec::<u8, _>::new_in(&secondary);
    assert!(v.try_reserve(1usize << (primary_limit - 1)).is_ok());

    let mut v = Vec::<u8, _>::new_in(&file);
    assert!(v.try_reserve(1usize << secondary_limit).is_err());
}
