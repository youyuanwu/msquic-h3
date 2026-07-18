//! Runtime native-library attestation gate (MF-3).
//!
//! Before the release conformance suite runs, [`tests::native_version_preflight`]
//! attests the library the test process **actually loaded**, two ways:
//!
//! 1. **Live-handle identity (primary).** Query, through the live MsQuic handle
//!    this process opened, `QUIC_PARAM_GLOBAL_LIBRARY_VERSION` (`[u32; 4]` =
//!    `[major, minor, patch, build]`) **and** `QUIC_PARAM_GLOBAL_LIBRARY_GIT_HASH`
//!    (`char[64]`, NUL-terminated). Both values are produced by code inside the
//!    loaded image, so they attest the running library regardless of file path.
//! 2. **Loaded-artifact digest (secondary, Linux).** Parse `/proc/self/maps` for
//!    the `libmsquic.so*` the loader actually mapped and record its SHA-256. On
//!    Linux this is a **blocking** part of the gate: if the loaded path/digest
//!    cannot be resolved the attestation FAILS (a missing digest cannot attest
//!    the loaded binary). Platforms without the digest mechanism (e.g. Windows)
//!    are an explicit, separately-scoped skip, never a silent Linux pass.
//!
//! The version gate is **row-specific** (Phase 9 decision, overriding MF-3's
//! single `[2,5,1]`): `native-find` links the system package (Linux CI installs
//! **2.5.8**) → expect `[2,5,8]`; `native-src` builds the vendored
//! `msquic-2.5.1-beta` source → expect `[2,5,1]`. `MSQUIC_EXPECTED_VERSION`
//! (`"major.minor.patch"`) overrides the per-feature default so any supported
//! matrix cell can pin its own value.
//!
//! This module is compiled only under `#[cfg(test)]`.

use msquic::{Api, Registration, RegistrationConfig};

/// Open a throwaway registration purely to force the native library to load and
/// initialize the global API table, so the global `get_param` queries below have
/// a live handle to answer them.
fn ensure_loaded() -> Registration {
    Registration::new(&RegistrationConfig::new())
        .expect("open msquic registration to load the native library")
}

/// Query `QUIC_PARAM_GLOBAL_LIBRARY_VERSION` = `[major, minor, patch, build]`
/// through the live handle the test process is using.
pub(crate) fn library_version() -> [u32; 4] {
    let _reg = ensure_loaded();
    // SAFETY: a global param (null handle) whose FFI type is `uint32_t[4]`, which
    // `[u32; 4]` matches exactly; the library is loaded via `_reg` above.
    unsafe {
        Api::get_param_auto::<[u32; 4]>(
            std::ptr::null_mut(),
            msquic::ffi::QUIC_PARAM_GLOBAL_LIBRARY_VERSION,
        )
    }
    .expect("query QUIC_PARAM_GLOBAL_LIBRARY_VERSION")
}

/// Query `QUIC_PARAM_GLOBAL_LIBRARY_GIT_HASH` (`char[64]`, NUL-terminated) and
/// return it as a UTF-8 string with any trailing NUL padding removed.
pub(crate) fn library_git_hash() -> String {
    let _reg = ensure_loaded();
    // SAFETY: a global param (null handle) whose FFI type is `char[64]`, matched
    // exactly by `[u8; 64]`; the library is loaded via `_reg` above.
    let raw = unsafe {
        Api::get_param_auto::<[u8; 64]>(
            std::ptr::null_mut(),
            msquic::ffi::QUIC_PARAM_GLOBAL_LIBRARY_GIT_HASH,
        )
    }
    .expect("query QUIC_PARAM_GLOBAL_LIBRARY_GIT_HASH");
    let end = raw.iter().position(|&b| b == 0).unwrap_or(raw.len());
    String::from_utf8_lossy(&raw[..end]).into_owned()
}

/// Resolve the `libmsquic.so*` the loader actually mapped into this process by
/// parsing `/proc/self/maps`, and return `(path, sha256_hex)`.
///
/// Ties the digest to the image the loader resolved — an installed-package digest
/// alone would not prove the process loaded *that* file.
#[cfg(target_os = "linux")]
pub(crate) fn loaded_libmsquic_digest() -> Option<(String, String)> {
    let maps = std::fs::read_to_string("/proc/self/maps").ok()?;
    let path = maps.lines().find_map(|line| {
        let p = line.split_whitespace().last()?;
        // Match the mapped shared object (e.g. `libmsquic.so.2`, possibly a
        // symlink target under `/usr/lib/...` or `$OUT_DIR/lib/...`).
        if p.rsplit('/')
            .next()
            .unwrap_or(p)
            .starts_with("libmsquic.so")
        {
            Some(p.to_string())
        } else {
            None
        }
    })?;
    let bytes = std::fs::read(&path).ok()?;
    Some((path, sha256_hex(&bytes)))
}

/// Minimal, dependency-free SHA-256 (FIPS 180-4) over a byte slice, returning the
/// lowercase hex digest. Test-only; used solely to record the loaded-artifact
/// digest, so it favors clarity over throughput.
#[cfg(target_os = "linux")]
fn sha256_hex(input: &[u8]) -> String {
    const K: [u32; 64] = [
        0x428a2f98, 0x71374491, 0xb5c0fbcf, 0xe9b5dba5, 0x3956c25b, 0x59f111f1, 0x923f82a4,
        0xab1c5ed5, 0xd807aa98, 0x12835b01, 0x243185be, 0x550c7dc3, 0x72be5d74, 0x80deb1fe,
        0x9bdc06a7, 0xc19bf174, 0xe49b69c1, 0xefbe4786, 0x0fc19dc6, 0x240ca1cc, 0x2de92c6f,
        0x4a7484aa, 0x5cb0a9dc, 0x76f988da, 0x983e5152, 0xa831c66d, 0xb00327c8, 0xbf597fc7,
        0xc6e00bf3, 0xd5a79147, 0x06ca6351, 0x14292967, 0x27b70a85, 0x2e1b2138, 0x4d2c6dfc,
        0x53380d13, 0x650a7354, 0x766a0abb, 0x81c2c92e, 0x92722c85, 0xa2bfe8a1, 0xa81a664b,
        0xc24b8b70, 0xc76c51a3, 0xd192e819, 0xd6990624, 0xf40e3585, 0x106aa070, 0x19a4c116,
        0x1e376c08, 0x2748774c, 0x34b0bcb5, 0x391c0cb3, 0x4ed8aa4a, 0x5b9cca4f, 0x682e6ff3,
        0x748f82ee, 0x78a5636f, 0x84c87814, 0x8cc70208, 0x90befffa, 0xa4506ceb, 0xbef9a3f7,
        0xc67178f2,
    ];

    let mut h: [u32; 8] = [
        0x6a09e667, 0xbb67ae85, 0x3c6ef372, 0xa54ff53a, 0x510e527f, 0x9b05688c, 0x1f83d9ab,
        0x5be0cd19,
    ];

    // Pre-processing: append 0x80, pad with zeros, then the 64-bit bit length.
    let bit_len = (input.len() as u64).wrapping_mul(8);
    let mut msg = input.to_vec();
    msg.push(0x80);
    while msg.len() % 64 != 56 {
        msg.push(0);
    }
    msg.extend_from_slice(&bit_len.to_be_bytes());

    for chunk in msg.chunks_exact(64) {
        let mut w = [0u32; 64];
        for (i, word) in w.iter_mut().take(16).enumerate() {
            *word = u32::from_be_bytes([
                chunk[i * 4],
                chunk[i * 4 + 1],
                chunk[i * 4 + 2],
                chunk[i * 4 + 3],
            ]);
        }
        for i in 16..64 {
            let s0 = w[i - 15].rotate_right(7) ^ w[i - 15].rotate_right(18) ^ (w[i - 15] >> 3);
            let s1 = w[i - 2].rotate_right(17) ^ w[i - 2].rotate_right(19) ^ (w[i - 2] >> 10);
            w[i] = w[i - 16]
                .wrapping_add(s0)
                .wrapping_add(w[i - 7])
                .wrapping_add(s1);
        }

        let [mut a, mut b, mut c, mut d, mut e, mut f, mut g, mut hh] = h;
        for i in 0..64 {
            let s1 = e.rotate_right(6) ^ e.rotate_right(11) ^ e.rotate_right(25);
            let ch = (e & f) ^ ((!e) & g);
            let t1 = hh
                .wrapping_add(s1)
                .wrapping_add(ch)
                .wrapping_add(K[i])
                .wrapping_add(w[i]);
            let s0 = a.rotate_right(2) ^ a.rotate_right(13) ^ a.rotate_right(22);
            let maj = (a & b) ^ (a & c) ^ (b & c);
            let t2 = s0.wrapping_add(maj);
            hh = g;
            g = f;
            f = e;
            e = d.wrapping_add(t1);
            d = c;
            c = b;
            b = a;
            a = t1.wrapping_add(t2);
        }

        for (dst, v) in h.iter_mut().zip([a, b, c, d, e, f, g, hh]) {
            *dst = dst.wrapping_add(v);
        }
    }

    let mut out = String::with_capacity(64);
    for word in h {
        out.push_str(&format!("{word:08x}"));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Row-specific expected `[major, minor, patch]`. `MSQUIC_EXPECTED_VERSION`
    /// (`"a.b.c"`) overrides; otherwise the committed provenance feature decides.
    fn expected_version() -> [u32; 3] {
        if let Ok(s) = std::env::var("MSQUIC_EXPECTED_VERSION") {
            let parts: Vec<u32> = s
                .split('.')
                .map(|p| {
                    p.trim()
                        .parse()
                        .expect("MSQUIC_EXPECTED_VERSION component not a u32")
                })
                .collect();
            assert!(
                parts.len() >= 3,
                "MSQUIC_EXPECTED_VERSION must be 'major.minor.patch', got {s:?}"
            );
            return [parts[0], parts[1], parts[2]];
        }
        // `native-find` and `native-src` are mutually exclusive; `cfg!` avoids the
        // unreachable-code lint a `#[cfg]`-return ladder would trip under
        // `-D warnings`.
        if cfg!(feature = "native-find") {
            [2, 5, 8]
        } else if cfg!(feature = "native-src") {
            [2, 5, 1]
        } else {
            // No provenance selected: the link step would already have failed, so
            // this branch is only reached by `cargo check`. Keep the source pin.
            [2, 5, 1]
        }
    }

    /// BLOCKING native-support gate: the loaded library's major/minor/patch MUST
    /// equal the row-specific expected version. Always records
    /// version + git-hash + loaded-artifact digest for the job's attestation.
    #[test]
    fn native_version_preflight() {
        let v = library_version();
        let git = library_git_hash();
        let expected = expected_version();

        println!("msquic attestation: version={v:?} (major.minor.patch.build) git_hash={git:?}");
        // Loaded-artifact digest is a BLOCKING part of the gate on Linux: a
        // missing path/digest means we cannot attest the binary the loader
        // actually mapped, so the gate MUST fail (a silent pass would defeat the
        // point). Platforms where the digest mechanism is intentionally not
        // implemented (e.g. Windows) never reach this block — that is an
        // explicit, separately-scoped skip via `#[cfg(target_os = "linux")]`,
        // NOT a silent pass on Linux.
        #[cfg(target_os = "linux")]
        {
            let (path, digest) = loaded_libmsquic_digest().expect(
                "msquic attestation FAILED: could not resolve the loaded libmsquic.so \
                 path/digest from /proc/self/maps — the loaded native binary cannot be \
                 attested, so the gate fails rather than passing silently",
            );
            println!("msquic loaded artifact: path={path} sha256={digest}");
        }

        assert_eq!(
            [v[0], v[1], v[2]],
            expected,
            "loaded libmsquic {v:?} major/minor/patch != expected {expected:?} \
             (build id v[3]={} ignored); check the provenance feature / \
             MSQUIC_EXPECTED_VERSION for this matrix cell",
            v[3]
        );
        assert!(!git.is_empty(), "git hash attestation must be non-empty");
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn sha256_known_answer() {
        // FIPS 180-4 example vectors — guards the hand-rolled digest.
        assert_eq!(
            sha256_hex(b""),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
        assert_eq!(
            sha256_hex(b"abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }
}
