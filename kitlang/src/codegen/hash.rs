//! Shared, deterministic hashing helpers.
//!
//! Used for generating stable, collision-resistant suffixes in generated C
//! identifiers (e.g. per-module header filenames and anonymous tuple struct
//! names). We intentionally use a single algorithm everywhere so the codebase
//! does not accumulate redundant hash implementations.

/// Compute a DJB2 hash over the given bytes, returned as a fixed-width
/// 16-character lowercase hex string (`{:016x}` of a `u64`).
///
/// DJB2 (Bernstein) with the `h = h * 33 + byte` step; the 64-bit accumulator
/// avoids overflow and yields a stable per-shape identifier across platforms.
pub(crate) fn djb2_hash(data: &[u8]) -> String {
    let mut h: u64 = 5381;
    for b in data {
        h = h.wrapping_mul(33).wrapping_add(u64::from(*b));
    }
    format!("{:016x}", h)
}

/// Convenience wrapper that hashes a string slice.
pub(crate) fn djb2_str<S: AsRef<str>>(s: S) -> String {
    djb2_hash(s.as_ref().as_bytes())
}
