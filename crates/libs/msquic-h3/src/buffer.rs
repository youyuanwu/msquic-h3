use std::ffi::c_void;

use bytes::Buf;
use msquic::BufferRef;

// Assumes buf does not free memory after advance
pub struct H3Buff<B: Buf> {
    inner: Box<(Vec<BufferRef>, B)>,
}

impl<B: Buf> H3Buff<B> {
    /// Convert buf slices into buffer ref
    /// zero copy. This assumes Buf does not free memory.
    pub fn new(inner: B) -> Self {
        let meta = vec![];
        let mut inner = Box::new((meta, inner));
        while inner.1.has_remaining() {
            let chunk = inner.1.chunk();
            // skip 0 buff
            if chunk.is_empty() {
                continue;
            }
            inner.0.push(BufferRef::from(chunk));
            inner.1.advance(chunk.len());
        }
        Self { inner }
    }

    /// detach
    /// # Safety
    /// Must be reattached
    pub unsafe fn into_raw(self) -> (&'static [BufferRef], *mut c_void) {
        let inner = self.inner;
        // unsafe buffers.
        let buffers = unsafe { std::slice::from_raw_parts(inner.0.as_ptr(), inner.0.len()) };
        let ptr = Box::into_raw(inner) as *mut _;
        (buffers, ptr)
    }

    /// attach
    /// # Safety
    /// ptr must from into_raw
    pub unsafe fn from_raw(ptr: *mut c_void) -> Self {
        let inner = Box::from_raw(ptr as *mut (Vec<BufferRef>, B));
        Self { inner }
    }
}

#[cfg(test)]
mod test {
    use bytes::{buf::Chain, Buf, Bytes};

    use super::H3Buff;

    #[test]
    fn buff_test() {
        let b = Bytes::from("hello").chain(Bytes::from("world"));
        let wrap = H3Buff::new(b);
        let (buff_ref, ptr) = unsafe { wrap.into_raw() };
        assert_eq!(buff_ref.len(), 2);
        assert_eq!(buff_ref[0].as_bytes(), b"hello");
        assert_eq!(buff_ref[1].as_bytes(), b"world");
        let wrap2: H3Buff<Chain<Bytes, Bytes>> = unsafe { H3Buff::from_raw(ptr) };
        assert_eq!(wrap2.inner.0[0].as_bytes(), b"hello");
        assert_eq!(wrap2.inner.0[1].as_bytes(), b"world");
    }
}
