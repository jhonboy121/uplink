//! Process CPU time from `/proc/self/stat`.

/// `utime`/`stime` positions among the fields following the `(comm)` entry (proc(5) fields 14/15).
const UTIME_AFTER_COMM: usize = 11;
const STIME_AFTER_COMM: usize = 12;

/// User + system CPU seconds consumed by this process, if readable.
pub fn process_seconds() -> Option<f64> {
    let stat = std::fs::read_to_string("/proc/self/stat").ok()?;
    let (_, rest) = stat.rsplit_once(')')?;
    let mut fields = rest.split_whitespace();
    let utime: f64 = fields.nth(UTIME_AFTER_COMM)?.parse().ok()?;
    let stime: f64 = fields.nth(STIME_AFTER_COMM - UTIME_AFTER_COMM - 1)?.parse().ok()?;
    // SAFETY: sysconf has no preconditions.
    let ticks_per_second = unsafe { libc::sysconf(libc::_SC_CLK_TCK) };
    (ticks_per_second > 0).then(|| (utime + stime) / ticks_per_second as f64)
}
