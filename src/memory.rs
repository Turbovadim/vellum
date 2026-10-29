//! A global allocator that maps large blocks straight from the OS on macOS.
//!
//! Page bitmaps and the rasterizer's scratch buffers are large and short-lived. macOS's
//! malloc keeps freed large blocks cached in the process, so they keep counting towards
//! the app's memory long after the bitmaps are gone. Mapping them directly returns them
//! the moment they are freed (glibc and the Windows heap already do this by default).

#[cfg(target_os = "macos")]
mod large_mmap {
    use std::alloc::{GlobalAlloc, Layout, System};
    use std::ptr;

    /// Allocations at least this big bypass malloc.
    const THRESHOLD: usize = 1 << 20;
    /// mmap returns memory aligned to at least a 4 KiB page.
    const MAX_ALIGN: usize = 4096;

    pub struct LargeMmap;

    fn is_large(layout: &Layout) -> bool {
        layout.size() >= THRESHOLD && layout.align() <= MAX_ALIGN
    }

    unsafe impl GlobalAlloc for LargeMmap {
        unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
            if !is_large(&layout) {
                return unsafe { System.alloc(layout) };
            }
            let p = unsafe {
                libc::mmap(
                    ptr::null_mut(),
                    layout.size(),
                    libc::PROT_READ | libc::PROT_WRITE,
                    libc::MAP_PRIVATE | libc::MAP_ANON,
                    -1,
                    0,
                )
            };
            if p == libc::MAP_FAILED {
                ptr::null_mut()
            } else {
                p.cast()
            }
        }

        unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
            if !is_large(&layout) {
                return unsafe { System.alloc_zeroed(layout) };
            }
            // Fresh anonymous mappings are already zeroed.
            unsafe { self.alloc(layout) }
        }

        unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
            if is_large(&layout) {
                unsafe { libc::munmap(ptr.cast(), layout.size()) };
            } else {
                unsafe { System.dealloc(ptr, layout) };
            }
        }

        unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
            let new_layout = unsafe { Layout::from_size_align_unchecked(new_size, layout.align()) };
            if !is_large(&layout) && !is_large(&new_layout) {
                return unsafe { System.realloc(ptr, layout, new_size) };
            }
            let new = unsafe { self.alloc(new_layout) };
            if !new.is_null() {
                unsafe {
                    ptr::copy_nonoverlapping(ptr, new, layout.size().min(new_size));
                    self.dealloc(ptr, layout);
                }
            }
            new
        }
    }
}

#[cfg(target_os = "macos")]
#[global_allocator]
static ALLOCATOR: large_mmap::LargeMmap = large_mmap::LargeMmap;
