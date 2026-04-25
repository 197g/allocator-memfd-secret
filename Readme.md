An allocator with backing memory in a `memfd_secret`.

Mainly written with [`allocator_api2`] in mind and thus compatible with the
next generation of Rust allocator data structures. That also allows opt-in
compatibility with the builtin traits on a nightly `rustc` build (see its
documentation).

[`allocator_api2`]: https://crates.io/crates/allocator_api2

# Usage

```rust
use allocator_memfd_secret::SecretArena;
use allocator_api2::boxed::Box;
use core::mem::MaybeUninit;

let file = SecretArena::new().unwrap();

let boxed_value = Box::new_in(5, &file);
let slice = Box::<[MaybeUninit<u8>], _>::new_zeroed_slice_in(1024, &file);
```
