// SPDX-License-Identifier: GPL-2.0-only

//! The daemon's allocator: the system's, except that large blocks are
//! taken from the system directly and given back to it the moment they
//! are freed.
//!
//! The system allocator on macOS keeps the room of a large block after
//! it is freed, for the next one. A long ISO-TP or J1939 message passes
//! through several such blocks, and after a few messages of a hundred
//! megabytes the daemon held several hundred megabytes that nothing
//! used, for as long as it ran. Asking the allocator to let go
//! (`malloc_zone_pressure_relief`) did not change that. Blocks taken
//! with `mmap` are not the allocator's to keep.
//!
//! Only blocks of at least [`LARGE`] bytes go this way: nothing on the
//! frame path is that big.

use std::alloc::{GlobalAlloc, Layout, System};

/// Blocks of at least this size are taken from the system directly.
pub const LARGE: usize = 1 << 20;

pub struct LargeAlloc;

#[cfg(target_os = "macos")]
mod map {
    use std::ffi::{c_int, c_void};

    extern "C" {
        fn mmap(
            addr: *mut c_void,
            len: usize,
            prot: c_int,
            flags: c_int,
            fd: c_int,
            offset: i64,
        ) -> *mut c_void;
        fn munmap(addr: *mut c_void, len: usize) -> c_int;
    }
    const PROT_READ: c_int = 0x01;
    const PROT_WRITE: c_int = 0x02;
    const MAP_PRIVATE: c_int = 0x0002;
    const MAP_ANON: c_int = 0x1000;
    /// Pages are at least this big on every Mac, so a mapping is aligned
    /// for anything that asks for no more.
    pub const ALIGNED_TO: usize = 4096;

    /// New zeroed memory, or null.
    pub unsafe fn take(len: usize) -> *mut u8 {
        let p = mmap(
            std::ptr::null_mut(),
            len,
            PROT_READ | PROT_WRITE,
            MAP_PRIVATE | MAP_ANON,
            -1,
            0,
        );
        if p as isize == -1 {
            std::ptr::null_mut()
        } else {
            p.cast()
        }
    }

    pub unsafe fn give_back(p: *mut u8, len: usize) {
        munmap(p.cast(), len);
    }
}

#[cfg(target_os = "macos")]
fn direct(layout: &Layout) -> bool {
    layout.size() >= LARGE && layout.align() <= map::ALIGNED_TO
}

// SAFETY: every block is freed the way it was taken, which its layout
// says: the same test of the same layout decides both. A block that
// changes size is moved to a new one taken for its new layout.
#[cfg(target_os = "macos")]
unsafe impl GlobalAlloc for LargeAlloc {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        if direct(&layout) {
            map::take(layout.size())
        } else {
            System.alloc(layout)
        }
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        if direct(&layout) {
            // New mappings are zero.
            map::take(layout.size())
        } else {
            System.alloc_zeroed(layout)
        }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        if direct(&layout) {
            map::give_back(ptr, layout.size());
        } else {
            System.dealloc(ptr, layout);
        }
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        let new_layout = Layout::from_size_align_unchecked(new_size, layout.align());
        if !direct(&layout) && !direct(&new_layout) {
            return System.realloc(ptr, layout, new_size);
        }
        let new = self.alloc(new_layout);
        if !new.is_null() {
            std::ptr::copy_nonoverlapping(ptr, new, layout.size().min(new_size));
            self.dealloc(ptr, layout);
        }
        new
    }
}

#[cfg(not(target_os = "macos"))]
unsafe impl GlobalAlloc for LargeAlloc {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        System.alloc(layout)
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        System.dealloc(ptr, layout)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn blocks_of_every_size_are_taken_grown_shrunk_and_freed() {
        let a = LargeAlloc;
        // Small, large, and across the line both ways.
        for (first, second) in [
            (100, 200),
            (LARGE, 3 * LARGE),
            (1000, 2 * LARGE),
            (2 * LARGE, 1000),
        ] {
            // SAFETY: each block is used within its size and freed with
            // the layout it has at that time.
            unsafe {
                let layout = Layout::from_size_align(first, 8).unwrap();
                let p = a.alloc(layout);
                assert!(!p.is_null());
                for i in 0..first {
                    p.add(i).write(i as u8);
                }
                let q = a.realloc(p, layout, second);
                assert!(!q.is_null());
                for i in 0..first.min(second) {
                    assert_eq!(q.add(i).read(), i as u8);
                }
                a.dealloc(q, Layout::from_size_align(second, 8).unwrap());
                let z = a.alloc_zeroed(layout);
                assert!((0..first).all(|i| z.add(i).read() == 0));
                a.dealloc(z, layout);
            }
        }
    }
}
