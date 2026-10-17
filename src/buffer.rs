//! Page-aligned byte buffers for plot reads.
//!
//! Direct I/O (`O_DIRECT` on Linux, `FILE_FLAG_NO_BUFFERING` on Windows)
//! requires the user buffer, the file offset and the transfer length to be
//! aligned to the device block size.  A plain `Vec<u8>` only guarantees
//! single-byte alignment, which happens to work for large allocations most of
//! the time and then fails with `EINVAL` on the one machine where glibc hands
//! out a heap chunk instead of a fresh mapping.  This type makes the alignment
//! explicit and keeps allocation and deallocation layouts identical.

use std::alloc::{alloc_zeroed, dealloc, Layout};
use std::ops::{Deref, DerefMut};
use std::ptr::NonNull;

/// Alignment used for every read buffer.  4 KiB satisfies the requirements of
/// 512e and 4Kn drives alike and matches the page size on every target we
/// support.
pub const BUFFER_ALIGNMENT: usize = 4096;

pub struct AlignedBuffer {
    ptr: NonNull<u8>,
    len: usize,
    layout: Option<Layout>,
}

// SAFETY: the buffer owns its allocation exclusively; sharing it across
// threads is as safe as sharing a `Vec<u8>`.
unsafe impl Send for AlignedBuffer {}
unsafe impl Sync for AlignedBuffer {}

impl AlignedBuffer {
    /// Allocate a zeroed buffer of `len` bytes aligned to [`BUFFER_ALIGNMENT`].
    pub fn new(len: usize) -> Self {
        if len == 0 {
            return Self {
                ptr: NonNull::dangling(),
                len: 0,
                layout: None,
            };
        }
        let layout = Layout::from_size_align(len, BUFFER_ALIGNMENT)
            .expect("buffer size overflows the allocator limits");
        // SAFETY: layout has non-zero size.
        let raw = unsafe { alloc_zeroed(layout) };
        let ptr = NonNull::new(raw).unwrap_or_else(|| std::alloc::handle_alloc_error(layout));
        Self {
            ptr,
            len,
            layout: Some(layout),
        }
    }

    /// Wrap memory owned by someone else (e.g. an OpenCL mapped buffer).
    /// The memory is never freed by this type.
    ///
    /// # Safety
    /// `ptr` must be valid for reads and writes of `len` bytes for as long
    /// as the returned buffer is alive, and nothing else may access it then.
    #[allow(dead_code)]
    pub unsafe fn from_raw_parts_unowned(ptr: *mut u8, len: usize) -> Self {
        Self {
            ptr: NonNull::new(ptr).expect("non-null pointer"),
            len,
            layout: None,
        }
    }

    pub fn len(&self) -> usize {
        self.len
    }

    #[allow(dead_code)]
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }
}

impl Deref for AlignedBuffer {
    type Target = [u8];
    fn deref(&self) -> &[u8] {
        // SAFETY: ptr is valid for `len` initialised bytes for the lifetime of self.
        unsafe { std::slice::from_raw_parts(self.ptr.as_ptr(), self.len) }
    }
}

impl DerefMut for AlignedBuffer {
    fn deref_mut(&mut self) -> &mut [u8] {
        // SAFETY: as above, and we hold the only mutable reference.
        unsafe { std::slice::from_raw_parts_mut(self.ptr.as_ptr(), self.len) }
    }
}

impl Drop for AlignedBuffer {
    fn drop(&mut self) {
        if let Some(layout) = self.layout {
            // SAFETY: allocated with exactly this layout in `new`.
            unsafe { dealloc(self.ptr.as_ptr(), layout) };
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn buffers_are_aligned_and_zeroed() {
        for len in [1usize, 64, 4096, 16 * 1024 * 1024] {
            let buf = AlignedBuffer::new(len);
            assert_eq!(buf.len(), len);
            assert_eq!(buf.as_ptr() as usize % BUFFER_ALIGNMENT, 0);
            assert!(buf.iter().all(|&b| b == 0));
        }
    }

    #[test]
    fn empty_buffer_is_fine() {
        let mut buf = AlignedBuffer::new(0);
        assert!(buf.is_empty());
        assert_eq!(buf.deref_mut().len(), 0);
    }

    #[test]
    fn buffer_is_writable() {
        let mut buf = AlignedBuffer::new(128);
        buf[5] = 42;
        buf[127] = 7;
        assert_eq!(buf[5], 42);
        assert_eq!(buf[127], 7);
    }
}
