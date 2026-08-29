// Copyright 2021 TiKV Project Authors. Licensed under Apache-2.0.

//! This module will be compiled when it's either linux_x86 or linux_x86_64.

use std::cell::UnsafeCell;
use std::fs::read_to_string;
use std::io::ErrorKind;
use std::time::{Duration, Instant};

static TSC_STATE: TSCState = TSCState {
    is_tsc_available: UnsafeCell::new(false),
    tsc_level: UnsafeCell::new(TSCLevel::Unstable),
    nanos_per_cycle: UnsafeCell::new(1.0),
};

struct TSCState {
    is_tsc_available: UnsafeCell<bool>,
    tsc_level: UnsafeCell<TSCLevel>,
    nanos_per_cycle: UnsafeCell<f64>,
}

unsafe impl Sync for TSCState {}

#[small_ctor::ctor]
unsafe fn init() {
    let tsc_level = TSCLevel::get();
    let is_tsc_available = match &tsc_level {
        TSCLevel::Stable { .. } => true,
        TSCLevel::Unstable => false,
    };
    if is_tsc_available {
        *TSC_STATE.nanos_per_cycle.get() = 1_000_000_000.0 / tsc_level.cycles_per_second() as f64;
    }
    *TSC_STATE.is_tsc_available.get() = is_tsc_available;
    *TSC_STATE.tsc_level.get() = tsc_level;
    std::sync::atomic::fence(std::sync::atomic::Ordering::SeqCst);
}

#[inline]
pub(crate) fn is_tsc_available() -> bool {
    unsafe { *TSC_STATE.is_tsc_available.get() }
}

#[inline]
pub(crate) fn nanos_per_cycle() -> f64 {
    unsafe { *TSC_STATE.nanos_per_cycle.get() }
}

#[inline]
pub(crate) fn current_cycle() -> u64 {
    match unsafe { &*TSC_STATE.tsc_level.get() } {
        TSCLevel::Stable {
            cycles_from_anchor, ..
        } => tsc().wrapping_sub(*cycles_from_anchor),
        TSCLevel::Unstable => panic!("tsc is unstable"),
    }
}

enum TSCLevel {
    Stable {
        cycles_per_second: u64,
        cycles_from_anchor: u64,
    },
    Unstable,
}

impl TSCLevel {
    fn get() -> TSCLevel {
        if !is_tsc_stable() {
            return TSCLevel::Unstable;
        }

        let anchor = Instant::now();
        let Some((cps, cfa)) = cycles_per_sec(anchor) else {
            return TSCLevel::Unstable;
        };
        TSCLevel::Stable {
            cycles_per_second: cps,
            cycles_from_anchor: cfa,
        }
    }

    #[inline]
    fn cycles_per_second(&self) -> u64 {
        match self {
            TSCLevel::Stable {
                cycles_per_second, ..
            } => *cycles_per_second,
            TSCLevel::Unstable => panic!("tsc is unstable"),
        }
    }
}

/// If linux kernel detected TSCs are sync between CPUs, we can
/// rely on the result to say tsc is stable so that no need to
/// sync TSCs by ourselves.
fn is_tsc_stable() -> bool {
    has_invariant_tsc() || clock_source_has_tsc()
}

fn clock_source_has_tsc() -> bool {
    #[cfg(target_os = "linux")]
    {
        const CURRENT_CLOCKSOURCE: &str =
            "/sys/devices/system/clocksource/clocksource0/current_clocksource";
        const AVAILABLE_CLOCKSOURCE: &str =
            "/sys/devices/system/clocksource/clocksource0/available_clocksource";

        match read_to_string(CURRENT_CLOCKSOURCE) {
            Ok(content) => content.contains("tsc"),
            Err(e) if e.kind() == ErrorKind::NotFound => {
                // we only check `available_clocksource` iff `current_clocksource` not exists.
                read_to_string(AVAILABLE_CLOCKSOURCE)
                    .map(|s| s.contains("tsc"))
                    .unwrap_or(false)
            }
            Err(_) => false,
        }
    }

    #[cfg(not(target_os = "linux"))]
    false
}

/// Invariant TSC could make sure TSC got synced among multi CPUs.
/// They will be reset at same time, and run in same frequency.
/// But in some VM, the max Extended Function in CPUID is < 0x80000007,
/// we should enable TSC if the system clock source is TSC.
#[inline]
fn has_invariant_tsc() -> bool {
    #[cfg(target_arch = "x86")]
    use core::arch::x86::__cpuid;
    #[cfg(target_arch = "x86_64")]
    use core::arch::x86_64::__cpuid;

    #[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
    {
        let cpuid_invariant_tsc_bts = 1 << 8;
        __cpuid(0x80000000).eax >= 0x80000007
            && __cpuid(0x80000007).edx & cpuid_invariant_tsc_bts != 0
    }

    #[cfg(not(any(target_arch = "x86", target_arch = "x86_64")))]
    false
}

/// Returns (1) cycles per second and (2) cycles from anchor.
/// The result of subtracting `cycles_from_anchor` from newly fetched TSC
/// can be used to
///   1. readjust TSC to begin from zero
///   2. sync TSCs between all CPUs
fn cycles_per_sec(anchor: Instant) -> Option<(u64, u64)> {
    let (cps, last_monotonic, last_tsc) = _cycles_per_sec();
    let elapsed = last_monotonic.checked_duration_since(anchor)?;
    let cycles_from_anchor = project_cycles_from_anchor(cps, elapsed, last_tsc)?;

    Some((cps, cycles_from_anchor))
}

/// Projects a sampled TSC back to the monotonic anchor.
///
/// A VM or CPU migration can produce a TSC value smaller than the projected
/// elapsed cycles even when the platform reported an invariant TSC. Treat such
/// a sample as an unsuccessful calibration so initialization can use the
/// fallback clock instead of panicking in the static constructor.
fn project_cycles_from_anchor(
    cycles_per_second: u64,
    elapsed: Duration,
    last_tsc: u64,
) -> Option<u64> {
    const NANOS_PER_SECOND: u128 = 1_000_000_000;

    let elapsed_cycles = u128::from(cycles_per_second)
        .checked_mul(elapsed.as_nanos())?
        .checked_add(NANOS_PER_SECOND - 1)?
        / NANOS_PER_SECOND;
    let elapsed_cycles = u64::try_from(elapsed_cycles).ok()?;
    last_tsc.checked_sub(elapsed_cycles)
}

/// Returns (1) cycles per second, (2) last monotonic time and (3) associated tsc.
fn _cycles_per_sec() -> (u64, Instant, u64) {
    let mut cycles_per_sec;
    let mut last_monotonic;
    let mut last_tsc;
    let mut old_cycles = 0.0;

    'outer: loop {
        let (t1, tsc1) = monotonic_with_tsc();
        loop {
            let (t2, tsc2) = monotonic_with_tsc();
            last_monotonic = t2;
            last_tsc = tsc2;
            let elapsed_nanos = (t2 - t1).as_nanos();
            if elapsed_nanos > 10_000_000 {
                // Even with fence added in monotonic_with_tsc(), tsc2 < tsc1 is still possible
                // if the thread migrates to a different CPU core between samples
                // (cores may have slightly different TSC offsets). checked_sub
                // prevents overflow; we retry from the outer loop with fresh tsc1.
                let Some(delta) = tsc2.checked_sub(tsc1) else {
                    continue 'outer;
                };
                cycles_per_sec = delta as f64 * 1_000_000_000.0 / elapsed_nanos as f64;
                break;
            }
        }
        let delta = f64::abs(cycles_per_sec - old_cycles);
        if delta / cycles_per_sec < 0.00001 {
            break;
        }
        old_cycles = cycles_per_sec;
    }

    (cycles_per_sec.round() as u64, last_monotonic, last_tsc)
}

/// Try to get tsc and monotonic time at the same time. Due to
/// get interrupted in half way may happen, they aren't guaranteed
/// to represent the same instant.
fn monotonic_with_tsc() -> (Instant, u64) {
    let t = Instant::now();
    // RDTSC is not serializing; LFENCE ensures Instant::now() completes first.
    #[cfg(target_feature = "sse2")]
    {
        #[cfg(target_arch = "x86")]
        use std::arch::x86::_mm_lfence;
        #[cfg(target_arch = "x86_64")]
        use std::arch::x86_64::_mm_lfence;
        unsafe { _mm_lfence() };
    }
    #[cfg(not(target_feature = "sse2"))]
    {
        use std::sync::atomic::compiler_fence;
        use std::sync::atomic::Ordering;
        compiler_fence(Ordering::SeqCst);
    }
    (t, tsc())
}

#[inline]
fn tsc() -> u64 {
    #[cfg(target_arch = "x86")]
    use core::arch::x86::_rdtsc;
    #[cfg(target_arch = "x86_64")]
    use core::arch::x86_64::_rdtsc;

    unsafe { _rdtsc() }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn anchor_projection_rounds_partial_cycles_up() {
        assert_eq!(
            project_cycles_from_anchor(1, Duration::from_nanos(1), 10),
            Some(9)
        );
    }

    #[test]
    fn anchor_projection_rejects_an_inconsistent_tsc_sample() {
        assert_eq!(
            project_cycles_from_anchor(1_000_000_000, Duration::from_nanos(11), 10),
            None
        );
    }
}
