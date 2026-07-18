use bytes::Bytes;
use msquic::BufferRef;

use crate::error::MAX_ADAPTER_SEND;

/// Classification of a `send_data` payload's `remaining()` length against the
/// adapter's owned-copy ceiling ([`MAX_ADAPTER_SEND`]).
///
/// The split is against the ceiling, **not** the raw `u32` protocol maximum, so
/// the adapter never materializes a near-`u32::MAX` owned buffer just to have
/// MsQuic reject the aggregate. The classification is performed up front, before
/// any allocation, so an oversized request is rejected without a copy.
pub(crate) enum SendLen {
    /// Zero-length payload: a successful no-op — no allocation, no native send.
    Empty,
    /// `0 < len <= MAX_ADAPTER_SEND`. The ceiling is below `u32::MAX`, so the
    /// materialized length always fits the `u32` a [`BufferRef`] carries.
    NonEmpty,
    /// `len > MAX_ADAPTER_SEND`: reject with `OversizedSend` before any allocation.
    Oversized,
}

/// Classify a payload length against [`MAX_ADAPTER_SEND`] before any allocation.
pub(crate) fn classify_send_len(remaining: usize) -> SendLen {
    if remaining == 0 {
        SendLen::Empty
    } else if remaining as u64 > MAX_ADAPTER_SEND {
        SendLen::Oversized
    } else {
        SendLen::NonEmpty
    }
}

/// An owned, self-referential send payload handed to MsQuic across the FFI
/// boundary for the whole lifetime of one `Stream::send`.
///
/// `_bytes` owns the heap storage holding the complete wire representation;
/// `buffers[0]` is a [`BufferRef`] — a `#[repr(transparent)]` wrapper over
/// `QUIC_BUFFER { Length: u32, Buffer: *mut u8 }` — whose `Buffer` pointer aims
/// into that heap storage, **not** into this struct. Moving a [`Bytes`] moves
/// only its `(ptr, len, refcount)` triple and never the referenced heap bytes,
/// so `buffers[0]` stays valid across any move of the `SendBuffer` — including
/// `Box::into_raw` / `Box::from_raw` transferred through `client_context`. The
/// struct is self-referential only in the weak sense that `buffers` borrows the
/// stable heap allocation owned by `_bytes`; that allocation lives exactly as
/// long as `_bytes`, i.e. as long as the `SendBuffer` itself.
pub(crate) struct SendBuffer {
    buffers: [BufferRef; 1],
    /// Owns the heap storage that `buffers[0]` points into. Never read directly
    /// (its leading underscore keeps the crate's `-D warnings` policy happy); it
    /// exists purely to keep the referenced bytes alive for MsQuic's use.
    _bytes: Bytes,
    /// Test-only reclamation counter: incremented once when this buffer is
    /// dropped, so a seam test can prove the `Box<SendBuffer>` is reconstructed
    /// and dropped exactly once on the `SendComplete` / immediate-failure path.
    #[cfg(test)]
    drop_count: Option<std::sync::Arc<std::sync::atomic::AtomicUsize>>,
}

// SAFETY: `_bytes` owns immutable, `Send` heap storage; `buffers` holds only
// inert pointer/length metadata into that storage (`BufferRef` is `!Send` only
// because of its raw `*mut u8`). The box is transferred to MsQuic through
// `client_context` and reconstructed + dropped exactly once after `SendComplete`
// (or reclaimed by the caller on an immediate `Stream::send` failure). No Rust
// code accesses the bytes while native code owns the box, so dropping it on an
// MsQuic worker thread is sound.
unsafe impl Send for SendBuffer {}

// Compile-time guard mirroring the design's `assert_impl_all!(SendBuffer: Send)`:
// the `unsafe impl Send` above is load-bearing (the eager `Box::into_raw` ->
// `client_context` -> `Box::from_raw` transfer launders the box through a raw
// pointer, so the compiler cannot otherwise check the cross-thread drop).
const _: fn() = || {
    fn assert_send<T: Send>() {}
    assert_send::<SendBuffer>();
};

impl SendBuffer {
    /// Materialize a validated, non-empty payload into an owned `SendBuffer`.
    ///
    /// `bytes` must be non-empty and no longer than [`MAX_ADAPTER_SEND`] (both
    /// already guaranteed by [`classify_send_len`] at the call site), so the
    /// `usize as u32` length cast inside `BufferRef::from` never truncates.
    pub(crate) fn new(bytes: Bytes) -> Self {
        debug_assert!(!bytes.is_empty(), "SendBuffer requires a non-empty payload");
        debug_assert!(
            bytes.len() as u64 <= MAX_ADAPTER_SEND,
            "SendBuffer length must be within MAX_ADAPTER_SEND"
        );
        // Build the `BufferRef` from the heap slice *before* moving `bytes` into
        // the struct. The pointer targets the heap allocation, which does not move
        // when `bytes` is moved, so the reference stays valid for the struct's
        // whole life (including after the struct is boxed and its box leaked).
        let buffer = BufferRef::from(&bytes[..]);
        Self {
            buffers: [buffer],
            _bytes: bytes,
            #[cfg(test)]
            drop_count: None,
        }
    }

    /// The single `BufferRef` slice to hand to `Stream::send`.
    pub(crate) fn buffers(&self) -> &[BufferRef] {
        &self.buffers
    }
}

#[cfg(test)]
impl SendBuffer {
    /// Test constructor attaching a reclamation counter incremented on drop.
    pub(crate) fn new_counted(
        bytes: Bytes,
        counter: std::sync::Arc<std::sync::atomic::AtomicUsize>,
    ) -> Self {
        let mut sb = Self::new(bytes);
        sb.drop_count = Some(counter);
        sb
    }
}

#[cfg(test)]
impl Drop for SendBuffer {
    fn drop(&mut self) {
        if let Some(c) = &self.drop_count {
            c.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        }
    }
}

#[cfg(test)]
mod test {
    use bytes::Bytes;

    use super::{SendBuffer, SendLen, classify_send_len};
    use crate::error::MAX_ADAPTER_SEND;

    #[test]
    fn classify_send_len_boundaries() {
        assert!(matches!(classify_send_len(0), SendLen::Empty));
        assert!(matches!(classify_send_len(1), SendLen::NonEmpty));
        assert!(matches!(
            classify_send_len(MAX_ADAPTER_SEND as usize),
            SendLen::NonEmpty
        ));
        assert!(matches!(
            classify_send_len(MAX_ADAPTER_SEND as usize + 1),
            SendLen::Oversized
        ));
    }

    #[test]
    fn send_buffer_pointer_stable_after_boxing() {
        let bytes = Bytes::from_static(b"helloworld");
        let expected_ptr = bytes.as_ptr();
        let expected_len = bytes.len();

        let sb = SendBuffer::new(bytes);
        assert_eq!(sb.buffers().len(), 1);
        assert_eq!(sb.buffers()[0].as_bytes(), b"helloworld");
        assert_eq!(sb.buffers()[0].as_bytes().as_ptr(), expected_ptr);
        assert_eq!(sb.buffers()[0].as_bytes().len(), expected_len);

        // Move into a Box exactly as the send executor does, hand out the raw
        // pointer, then observe the BufferRef through it: the payload pointer and
        // length must survive the move into the box unchanged.
        let raw = Box::into_raw(Box::new(sb));
        // SAFETY: `raw` came from `Box::into_raw` above and is not aliased.
        let bufs = unsafe { (*raw).buffers() };
        assert_eq!(bufs[0].as_bytes(), b"helloworld");
        assert_eq!(bufs[0].as_bytes().as_ptr(), expected_ptr);
        assert_eq!(bufs[0].as_bytes().len(), expected_len);
        // Reclaim exactly once (mirrors the SendComplete / immediate-failure path).
        // SAFETY: `raw` is the pointer from `Box::into_raw`, reclaimed once here.
        drop(unsafe { Box::from_raw(raw) });
    }
}
