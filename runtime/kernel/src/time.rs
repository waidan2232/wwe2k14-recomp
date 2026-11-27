//! Clocks: system time (`FILETIME`, 100 ns since 1601), interrupt time, the
//! performance counter, and `KeTimeStampBundle`, which the kernel keeps
//! current and the game reads directly (XAPI's `GetTickCount`).

use std::sync::OnceLock;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use crate::mem;

/// 100 ns intervals from 1601-01-01 to 1970-01-01.
const EPOCH_DIFF: u64 = 116_444_736_000_000_000;

fn boot() -> Instant {
    static BOOT: OnceLock<Instant> = OnceLock::new();
    *BOOT.get_or_init(Instant::now)
}

/// Current system time as a `FILETIME` (UTC).
pub fn system_time() -> u64 {
    let d = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default();
    EPOCH_DIFF + (d.as_nanos() / 100) as u64
}

/// 100 ns units since boot.
/// Time since boot.
pub fn since_boot() -> Duration {
    boot().elapsed()
}

pub fn interrupt_time() -> u64 {
    (boot().elapsed().as_nanos() / 100) as u64
}

/// Frequency of `mftb`, which XAPI's `QueryPerformanceCounter` reads. It must
/// match `ppc_rt::timebase`.
pub const PERFORMANCE_FREQUENCY: u64 = 49_875_000;

/// Starts the 1 ms updater for `KeTimeStampBundle`
/// (`{ u64 InterruptTime; u64 SystemTime; u32 TickCount; u32 pad; }`).
pub fn start_bundle_updater(bundle: u32) {
    boot();
    update_bundle(bundle);
    std::thread::Builder::new()
        .name("kernel-clock".into())
        .spawn(move || {
            loop {
                std::thread::sleep(Duration::from_millis(1));
                update_bundle(bundle);
            }
        })
        .expect("clock thread");
}

fn update_bundle(bundle: u32) {
    mem::w64(bundle, interrupt_time());
    mem::w64(bundle + 8, system_time());
    mem::w32(bundle + 0x10, boot().elapsed().as_millis() as u32);
}

/// `TIME_FIELDS { u16 Year, Month, Day, Hour, Minute, Second, Milliseconds, Weekday; }`
pub struct TimeFields {
    pub year: u16,
    pub month: u16,
    pub day: u16,
    pub hour: u16,
    pub minute: u16,
    pub second: u16,
    pub millis: u16,
    pub weekday: u16,
}

/// Days from 1601-01-01 to the civil date, and back (proleptic Gregorian).
fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    // Howard Hinnant's algorithm, shifted from 1970 to 1601.
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let mp = (m + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468 + 134_774
}

fn civil_from_days(z: i64) -> (i64, i64, i64) {
    let z = z - 134_774 + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    (if m <= 2 { y + 1 } else { y }, m, d)
}

pub fn to_fields(t: u64) -> TimeFields {
    let ms_total = t / 10_000;
    let days = (ms_total / 86_400_000) as i64;
    let ms_day = ms_total % 86_400_000;
    let (y, m, d) = civil_from_days(days);
    TimeFields {
        year: y as u16,
        month: m as u16,
        day: d as u16,
        hour: (ms_day / 3_600_000) as u16,
        minute: (ms_day / 60_000 % 60) as u16,
        second: (ms_day / 1000 % 60) as u16,
        millis: (ms_day % 1000) as u16,
        // 1601-01-01 was a Monday.
        weekday: ((days + 1) % 7) as u16,
    }
}

pub fn from_fields(f: &TimeFields) -> Option<u64> {
    if !(1601..=30827).contains(&f.year)
        || !(1..=12).contains(&f.month)
        || f.day == 0
        || f.day > 31
        || f.hour > 23
        || f.minute > 59
        || f.second > 59
        || f.millis > 999
    {
        return None;
    }
    let days = days_from_civil(f.year as i64, f.month as i64, f.day as i64);
    let ms = days as u64 * 86_400_000
        + f.hour as u64 * 3_600_000
        + f.minute as u64 * 60_000
        + f.second as u64 * 1000
        + f.millis as u64;
    Some(ms * 10_000)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn time_fields_round_trip() {
        // 2013-10-29 12:34:56.789 (a Tuesday)
        let f = TimeFields { year: 2013, month: 10, day: 29, hour: 12, minute: 34, second: 56, millis: 789, weekday: 0 };
        let t = from_fields(&f).unwrap();
        let g = to_fields(t);
        assert_eq!((g.year, g.month, g.day, g.hour, g.minute, g.second, g.millis), (2013, 10, 29, 12, 34, 56, 789));
        assert_eq!(g.weekday, 2);
        // 1970-01-01 is the Unix epoch.
        let e = from_fields(&TimeFields { year: 1970, month: 1, day: 1, hour: 0, minute: 0, second: 0, millis: 0, weekday: 0 });
        assert_eq!(e, Some(EPOCH_DIFF));
        assert_eq!(to_fields(0).weekday, 1);
    }
}
