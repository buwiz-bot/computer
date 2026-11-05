//! Process-wide allocator for buwiz-computer guest daemons.
//!
//! buwiz-computer does **not** pick one allocator for every lifetime:
//!
//! - **Global / multi-thread / mixed size** (`box-exec`, `box-host`):
//!   [mimalloc](https://github.com/microsoft/mimalloc) via `#[global_allocator]`.
//!   Same role as in Bun: general-purpose, per-thread caches, low
//!   fragmentation. Not a bump arena.
//! - **Short-lived tiny lists** (key chords, drag waypoints): `ArrayVec` /
//!   `SmallVec`. Criterion: 25 waypoints are faster on `ArrayVec` than bumpalo
//!   `reset` (~5 ns vs ~58 ns). bumpalo stays a `box-cua` bench-only dep.
//! - **Reusable screenshot RGB:** `ShotConn.rgb` is kept when geometry matches.
//!   A fresh mimalloc heap / `mi_heap_destroy` is not wired — the `mimalloc`
//!   crate exposes `GlobalAlloc` only; raw `mi_heap_*` is unsafe reset and
//!   unused while RGB reuse already skips the 3 MiB fill.
//!
//! Default-on for `box-exec` / `box-host`. Disable with
//! `--no-default-features` if glibc malloc wins a measured hot path.

/// Name of the process-wide allocator, for logs.
pub const GLOBAL_ALLOCATOR: &str = if cfg!(feature = "mimalloc") {
    "mimalloc"
} else {
    "system"
};

#[cfg(feature = "mimalloc")]
#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn global_allocator_name_matches_feature() {
        #[cfg(feature = "mimalloc")]
        assert_eq!(GLOBAL_ALLOCATOR, "mimalloc");
        #[cfg(not(feature = "mimalloc"))]
        assert_eq!(GLOBAL_ALLOCATOR, "system");
        let v = [1u8, 2, 3];
        assert_eq!(v.len(), 3);
    }
}
