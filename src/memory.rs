//! How the index daemon (Linux) and service (Windows) share memory with the
//! rest of the machine: the same deal as CPU. They may use memory freely,
//! but give it back before anything else has to.
//!
//! Readings (available memory, how much other processes are stalled waiting
//! for memory, the OS's low-memory signal) map to a level:
//! * `Normal`: nothing to do; caches grow as they're useful.
//! * `Tight` (available below the floor, default 10%; others stalled over
//!   10% of the time; or the OS signals low memory): background work pauses,
//!   freed heap goes back to the OS, the mapped index is marked cold so it is
//!   reclaimed before other programs' memory.
//! * `Critical` (available below half the floor, or others stalled over 25%):
//!   rebuildable caches are dropped too and resident pages handed back.
//!
//! Levels drop back only once readings are clearly better, so the service
//! doesn't flap on a boundary.

/// Ordered: `Critical > Tight > Normal`.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
#[repr(u8)]
pub enum Level {
    Normal = 0,
    Tight = 1,
    Critical = 2,
}

impl Level {
    pub fn from_u8(v: u8) -> Level {
        match v {
            1 => Level::Tight,
            2 => Level::Critical,
            _ => Level::Normal,
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Level::Normal => "normal",
            Level::Tight => "tight",
            Level::Critical => "critical",
        }
    }
}

/// One look at the machine.
#[derive(Clone, Copy, Debug)]
pub struct Reading {
    /// Available memory as a fraction of total (0..1).
    pub available: f64,
    /// Percent of recent time other processes were stalled on memory (PSI
    /// `some avg10`, minus this process's own share where measurable).
    pub stall: f64,
    /// The OS says memory is low (Windows' low-memory notification).
    pub low: bool,
}

/// Default floor: below 10% available, the service starts giving memory back.
pub const DEFAULT_FLOOR: f64 = 0.10;

/// The next level, from the previous one and a reading.
pub fn level(prev: Level, r: &Reading, floor: f64) -> Level {
    let critical = r.available < floor / 2.0 || r.stall > 25.0;
    let tight = critical || r.available < floor || r.stall > 10.0 || r.low;
    // Clearly better: well above the floor, others barely stalled.
    let clear = r.available >= floor * 1.5 && r.stall < 5.0 && !r.low;
    match prev {
        Level::Critical if critical || r.available < floor || r.stall >= 12.0 => Level::Critical,
        _ if critical => Level::Critical,
        _ if tight => Level::Tight,
        Level::Tight | Level::Critical if !clear => Level::Tight,
        _ => Level::Normal,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn r(available: f64, stall: f64, low: bool) -> Reading {
        Reading { available, stall, low }
    }

    #[test]
    fn levels_and_hysteresis() {
        let f = DEFAULT_FLOOR;
        assert_eq!(level(Level::Normal, &r(0.5, 0.0, false), f), Level::Normal);
        assert_eq!(level(Level::Normal, &r(0.08, 0.0, false), f), Level::Tight);
        assert_eq!(level(Level::Normal, &r(0.5, 15.0, false), f), Level::Tight);
        assert_eq!(level(Level::Normal, &r(0.5, 0.0, true), f), Level::Tight);
        assert_eq!(level(Level::Normal, &r(0.04, 0.0, false), f), Level::Critical);
        assert_eq!(level(Level::Normal, &r(0.5, 30.0, false), f), Level::Critical);
        // Just above the floor isn't clearly better: stay tight.
        assert_eq!(level(Level::Tight, &r(0.12, 0.0, false), f), Level::Tight);
        assert_eq!(level(Level::Tight, &r(0.2, 0.0, false), f), Level::Normal);
        // Critical eases to tight first, then to normal.
        assert_eq!(level(Level::Critical, &r(0.07, 0.0, false), f), Level::Critical);
        assert_eq!(level(Level::Critical, &r(0.11, 0.0, false), f), Level::Tight);
        assert_eq!(level(Level::Critical, &r(0.3, 0.0, false), f), Level::Normal);
    }
}
