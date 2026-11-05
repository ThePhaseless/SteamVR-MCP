//! One session clock: `t_s` counts wall-clock seconds from T0, the MCP's start, so it agrees with `at` (the same instant as
//! RFC 3339 UTC) and with SteamVR's log times. It follows the system clock: a stepped clock makes it jump or go back.
use chrono::{DateTime, SecondsFormat, Utc};
use std::{
    sync::LazyLock,
    time::{Duration, SystemTime},
};

/// Forced once in `main`, so T0 is the MCP's start rather than the first use.
pub static T0: LazyLock<SystemTime> = LazyLock::new(SystemTime::now);

pub fn now_s() -> f64 {
    LazyLock::force(&T0); // before now(): a first call would otherwise read the clock before T0 and come out negative
    wall_t_s(SystemTime::now())
}

/// Signed seconds from T0's wall time.
pub fn wall_t_s(wall: SystemTime) -> f64 {
    match wall.duration_since(*T0) {
        Ok(d) => d.as_secs_f64(),
        Err(e) => -e.duration().as_secs_f64(),
    }
}

pub fn rfc3339(wall: SystemTime) -> String {
    DateTime::<Utc>::from(wall).to_rfc3339_opts(SecondsFormat::Millis, true)
}

pub fn parse_rfc3339(s: &str) -> Option<SystemTime> {
    DateTime::parse_from_rfc3339(s).ok().map(SystemTime::from)
}

/// The wall time of session second `t_s`.
pub fn at(t_s: f64) -> String {
    let offset = Duration::from_secs_f64(t_s.abs());
    rfc3339(if t_s < 0.0 { *T0 - offset } else { *T0 + offset })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, UNIX_EPOCH};

    #[test]
    fn rfc3339_is_utc_with_millis() {
        let wall = UNIX_EPOCH + Duration::from_millis(1_700_000_000_123);
        assert_eq!(rfc3339(wall), "2023-11-14T22:13:20.123Z");
        assert_eq!(parse_rfc3339("2023-11-14T22:13:20.123Z"), Some(wall));
    }

    #[test]
    fn at_is_t0_plus_t_s() {
        assert_eq!(at(1.5), rfc3339(*T0 + Duration::from_millis(1500)));
        assert_eq!(at(-1.5), rfc3339(*T0 - Duration::from_millis(1500)));
    }

    #[test]
    fn t_s_and_at_agree_with_the_wall_clock() {
        assert!((wall_t_s(SystemTime::now()) - now_s()).abs() < 0.05);
        let parsed = parse_rfc3339(&at(now_s())).unwrap();
        let off = parsed.duration_since(SystemTime::now()).unwrap_or_else(|e| e.duration());
        assert!(off < Duration::from_millis(50));
    }
}
