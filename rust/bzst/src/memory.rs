//! Host physical-memory query, used to bound per-block allocations so that a
//! corrupt or malicious size field produces a clean [`crate::BzstError::BlockTooLarge`]
//! error instead of an out-of-memory abort. There is no portable `std` API for
//! total RAM, so this uses localized platform FFI via `libc`.

use std::sync::OnceLock;

use crate::{BzstError, BzstResult};

/// Rejects a block whose combined compressed + uncompressed size exceeds `limit`,
/// so a corrupt or forged size field yields a clean [`BzstError::BlockTooLarge`]
/// instead of an out-of-memory abort. `limit` is typically [`default_alloc_limit`].
pub(crate) fn check_block_fits(compressed: u64, uncompressed: u64, limit: u64) -> BzstResult<()> {
    let requested = compressed.saturating_add(uncompressed);
    if requested > limit {
        Err(BzstError::BlockTooLarge { requested, limit })
    } else {
        Ok(())
    }
}

/// Fallback per-block allocation cap used when total physical memory can't be
/// queried (e.g. an unsupported platform). Generous enough for any realistic
/// block yet bounded, so protection is never fully disabled.
const FALLBACK_ALLOC_LIMIT: u64 = 8 << 30; // 8 GiB

/// A conservative cap on the bytes one block may require to decode (compressed +
/// uncompressed): 95% of total physical memory, queried once and cached. A block
/// that needs more than the host has cannot be decoded anyway, so rejecting it is
/// the correct outcome. When the platform total can't be determined, falls back
/// to [`FALLBACK_ALLOC_LIMIT`] rather than disabling the cap, so a corrupt size
/// field still can't drive an unbounded allocation.
pub(crate) fn default_alloc_limit() -> u64 {
    static LIMIT: OnceLock<u64> = OnceLock::new();
    *LIMIT.get_or_init(|| match total_physical_memory() {
        // Divide before multiplying to keep well clear of u64 overflow.
        Some(total) => (total / 100) * 95,
        None => FALLBACK_ALLOC_LIMIT,
    })
}

#[cfg(target_os = "linux")]
fn total_physical_memory() -> Option<u64> {
    // SAFETY: `sysconf` takes an integer name and returns a `long`; no pointers
    // are involved, so the call cannot violate memory safety.
    let pages = unsafe { libc::sysconf(libc::_SC_PHYS_PAGES) };
    let page_size = unsafe { libc::sysconf(libc::_SC_PAGESIZE) };
    (pages > 0 && page_size > 0).then(|| pages as u64 * page_size as u64)
}

#[cfg(target_os = "macos")]
fn total_physical_memory() -> Option<u64> {
    let mut mem: u64 = 0;
    let mut len = std::mem::size_of::<u64>();
    // SAFETY: `hw.memsize` yields a 64-bit integer; we pass a pointer to a `u64`
    // and a matching length, a null new-value pointer, and check the return code
    // and that the kernel wrote the expected number of bytes.
    let rc = unsafe {
        libc::sysctlbyname(
            c"hw.memsize".as_ptr(),
            (&mut mem as *mut u64).cast::<libc::c_void>(),
            &mut len,
            std::ptr::null_mut(),
            0,
        )
    };
    (rc == 0 && len == std::mem::size_of::<u64>() && mem > 0).then_some(mem)
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn total_physical_memory() -> Option<u64> {
    None
}
