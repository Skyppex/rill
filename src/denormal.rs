//! Flush-to-zero for the duration of a render call.
//!
//! Denormal floats can be ~100x slower on some CPUs, which shows up as
//! dropouts in decaying tails. While a [`FlushDenormals`] guard is alive the
//! current thread treats them as zero; the previous mode is restored on drop.

pub struct FlushDenormals {
    #[allow(dead_code)]
    saved: usize,
}

#[cfg(target_arch = "x86_64")]
mod imp {
    use std::arch::asm;

    /// MXCSR flush-to-zero (FTZ) and denormals-are-zero (DAZ).
    const FLAGS: u32 = 0x8000 | 0x0040;

    pub fn enter() -> usize {
        let mut saved: u32 = 0;
        // SAFETY: reading and writing MXCSR only changes this thread's SSE
        // rounding/denormal mode, and SSE is baseline on x86_64.
        unsafe {
            asm!("stmxcsr [{}]", in(reg) &mut saved, options(nostack, preserves_flags));
            let flushed = saved | FLAGS;
            asm!("ldmxcsr [{}]", in(reg) &flushed, options(nostack, preserves_flags));
        }
        saved as usize
    }

    pub fn leave(saved: usize) {
        let saved = saved as u32;
        // SAFETY: restores the value read in `enter`.
        unsafe {
            asm!("ldmxcsr [{}]", in(reg) &saved, options(nostack, preserves_flags));
        }
    }
}

#[cfg(target_arch = "aarch64")]
mod imp {
    use std::arch::asm;

    /// FPCR.FZ
    const FZ: u64 = 1 << 24;

    pub fn enter() -> usize {
        let saved: u64;
        // SAFETY: FPCR is per-thread floating point control state.
        unsafe {
            asm!("mrs {}, fpcr", out(reg) saved, options(nomem, nostack, preserves_flags));
            asm!("msr fpcr, {}", in(reg) saved | FZ, options(nomem, nostack, preserves_flags));
        }
        saved as usize
    }

    pub fn leave(saved: usize) {
        // SAFETY: restores the value read in `enter`.
        unsafe {
            asm!("msr fpcr, {}", in(reg) saved as u64, options(nomem, nostack, preserves_flags));
        }
    }
}

#[cfg(not(any(target_arch = "x86_64", target_arch = "aarch64")))]
mod imp {
    pub fn enter() -> usize {
        0
    }

    pub fn leave(_: usize) {}
}

impl FlushDenormals {
    #[inline]
    #[allow(clippy::new_without_default)]
    pub fn new() -> Self {
        FlushDenormals {
            saved: imp::enter(),
        }
    }
}

impl Drop for FlushDenormals {
    #[inline]
    fn drop(&mut self) {
        imp::leave(self.saved);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::hint::black_box;

    #[cfg(any(target_arch = "x86_64", target_arch = "aarch64"))]
    #[test]
    fn denormals_flush_inside_guard_only() {
        let tiny = black_box(f32::MIN_POSITIVE);
        {
            let _g = FlushDenormals::new();
            assert_eq!(black_box(tiny * black_box(0.5)), 0.0);
        }
        assert!(black_box(tiny * black_box(0.5)) > 0.0);
    }
}
