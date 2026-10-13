//! Page-aligned buffers: `FILE_FLAG_NO_BUFFERING` requires the buffer address, the file offset and the I/O
//! length to be multiples of the sector size (4096 covers 512e and 4Kn drives).

use std::alloc::{Layout, alloc_zeroed, dealloc};
use std::ops::{Deref, DerefMut};
use std::ptr::NonNull;

pub const ALIGN: usize = 4096;

pub struct AlignedBuf {
    ptr: NonNull<u8>,
    len: usize,
}

// SAFETY: the buffer is uniquely owned heap memory; moving it between threads is fine.
unsafe impl Send for AlignedBuf {}

impl AlignedBuf {
    pub fn new(len: usize) -> AlignedBuf {
        assert!(len > 0 && len.is_multiple_of(ALIGN));
        let layout = Layout::from_size_align(len, ALIGN).expect("layout");
        // SAFETY: the layout has non-zero size.
        let p = unsafe { alloc_zeroed(layout) };
        let ptr = NonNull::new(p).unwrap_or_else(|| std::alloc::handle_alloc_error(layout));
        AlignedBuf { ptr, len }
    }
}

impl Deref for AlignedBuf {
    type Target = [u8];
    fn deref(&self) -> &[u8] {
        // SAFETY: `ptr` points to `len` initialised (zeroed) bytes owned by self.
        unsafe { std::slice::from_raw_parts(self.ptr.as_ptr(), self.len) }
    }
}

impl DerefMut for AlignedBuf {
    fn deref_mut(&mut self) -> &mut [u8] {
        // SAFETY: as above, and we hold `&mut self`.
        unsafe { std::slice::from_raw_parts_mut(self.ptr.as_ptr(), self.len) }
    }
}

impl Drop for AlignedBuf {
    fn drop(&mut self) {
        let layout = Layout::from_size_align(self.len, ALIGN).expect("layout");
        // SAFETY: allocated in `new` with the same layout.
        unsafe { dealloc(self.ptr.as_ptr(), layout) };
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn is_aligned_and_zeroed() {
        let mut b = AlignedBuf::new(8192);
        assert_eq!(b.as_ptr() as usize % ALIGN, 0);
        assert!(b.iter().all(|&x| x == 0));
        b[8191] = 7;
        assert_eq!(b[8191], 7);
    }
}
