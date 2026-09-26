//! How long something ran, as a clock reads it: minutes and seconds, and hours once there are any.

use std::time::Duration;

const SECONDS_PER_MINUTE: u64 = 60;
const MINUTES_PER_HOUR: u64 = 60;

/// A running call's clock: 04:12, then 1:15:03.
pub fn timer(elapsed: Duration) -> String {
    let (hours, minutes, seconds) = split(elapsed);
    if hours > 0 { format!("{hours}:{minutes:02}:{seconds:02}") } else { format!("{minutes:02}:{seconds:02}") }
}

/// How long a finished call lasted: 4:12, then 1:15:03.
pub fn length(elapsed: Duration) -> String {
    let (hours, minutes, seconds) = split(elapsed);
    if hours > 0 { format!("{hours}:{minutes:02}:{seconds:02}") } else { format!("{minutes}:{seconds:02}") }
}

const fn split(elapsed: Duration) -> (u64, u64, u64) {
    let total = elapsed.as_secs();
    let minutes = total / SECONDS_PER_MINUTE;
    (minutes / MINUTES_PER_HOUR, minutes % MINUTES_PER_HOUR, total % SECONDS_PER_MINUTE)
}

#[cfg(test)]
mod tests {
    use super::*;

    const fn seconds(n: u64) -> Duration {
        Duration::from_secs(n)
    }

    #[test]
    fn a_timer_pads_its_minutes_until_the_first_hour() {
        assert_eq!(timer(seconds(0)), "00:00");
        assert_eq!(timer(seconds(252)), "04:12");
        assert_eq!(timer(seconds(3_599)), "59:59");
    }

    #[test]
    fn a_length_does_not() {
        assert_eq!(length(seconds(5)), "0:05");
        assert_eq!(length(seconds(252)), "4:12");
        assert_eq!(length(seconds(3_599)), "59:59");
    }

    #[test]
    fn both_count_hours_once_there_are_any() {
        for format in [timer, length] {
            assert_eq!(format(seconds(3_600)), "1:00:00");
            assert_eq!(format(seconds(5_103)), "1:25:03");
            assert_eq!(format(seconds(36_061)), "10:01:01");
        }
    }
}
