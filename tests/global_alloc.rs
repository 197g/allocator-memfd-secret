use allocator_memfd_secret::SecretArena;
use core::alloc::{GlobalAlloc, Layout};
use std::sync::LazyLock;

struct LazySecret<F> {
    inner: LazyLock<SecretArena, F>,
}

unsafe impl<F: Fn() -> SecretArena> GlobalAlloc for LazySecret<F> {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        unsafe { self.inner.alloc(layout) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        unsafe { self.inner.dealloc(ptr, layout) }
    }
}

#[global_allocator]
static ALLOC: LazySecret<fn() -> SecretArena> = LazySecret {
    inner: LazyLock::new(|| {
        SecretArena::new().unwrap()
    }),
};

#[test]
fn works() {
    let _b: Box<[u8; 1024]> = Box::new([0; 1024]);
}
