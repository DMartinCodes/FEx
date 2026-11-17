//! How much CPU background indexing may claim on a fully loaded machine,
//! shared by the Linux daemon and the Windows service.
//!
//! Background work runs at the lowest priority, which on a machine that is
//! always busy means it never finishes. So a small share of total CPU
//! capacity is reserved for it under full load (it still takes everything
//! that's idle):
//! * 0.5% with few files per core (a compute server: many cores, a modest
//!   tree; the index is cheap to keep up);
//! * up to 5% with many files per core (a laptop or workstation holding
//!   millions of files on a few cores);
//! * on a log scale between 10K and 1M files per core, and a quarter more
//!   on machines with a battery (capped at 5%).
//!
//! A controller enforces it: it measures the CPU time background threads
//! actually get and steps their priority up while that is below the
//! reserve, and back down once it's above.

pub const MIN_RESERVE: f64 = 0.005;
pub const MAX_RESERVE: f64 = 0.05;

/// The reserve for a machine with `cores` cores and `files` files.
pub fn reserve_for(cores: usize, files: u64, battery: bool) -> f64 {
    let per_core = files as f64 / cores.max(1) as f64;
    let t = ((per_core.max(1.0).log10() - 4.0) / 2.0).clamp(0.0, 1.0);
    let r = MIN_RESERVE + t * (MAX_RESERVE - MIN_RESERVE);
    if battery { (r * 1.25).min(MAX_RESERVE) } else { r }
}

/// The controller's next priority step (0 = lowest, `max` = highest it may
/// raise to), given background work's recent share of total CPU.
pub fn next_step(step: usize, share: f64, reserve: f64, active: bool, max: usize) -> usize {
    if !active || reserve <= 0.0 {
        0
    } else if share < reserve * 0.8 {
        (step + 1).min(max)
    } else if share > reserve * 1.3 {
        step.saturating_sub(1)
    } else {
        step
    }
}

/// Smooths the measured share over about a second of 100 ms ticks.
pub fn smooth(previous: f64, sample: f64) -> f64 {
    0.6 * previous + 0.4 * sample
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reserve_scales_with_files_per_core() {
        assert_eq!(reserve_for(64, 500_000, false), MIN_RESERVE);
        let laptop = reserve_for(8, 2_000_000, true);
        assert!(laptop > 0.04 && laptop <= MAX_RESERVE, "{laptop}");
        assert_eq!(reserve_for(2, 5_000_000, false), MAX_RESERVE);
        assert!(reserve_for(16, 4_000_000, false) > reserve_for(16, 400_000, false));
    }

    #[test]
    fn steps_toward_the_reserve() {
        assert_eq!(next_step(0, 0.0, 0.01, true, 5), 1);
        assert_eq!(next_step(5, 0.0, 0.01, true, 5), 5);
        assert_eq!(next_step(3, 0.05, 0.01, true, 5), 2);
        assert_eq!(next_step(3, 0.01, 0.01, true, 5), 3);
        assert_eq!(next_step(3, 0.0, 0.01, false, 5), 0);
    }
}
