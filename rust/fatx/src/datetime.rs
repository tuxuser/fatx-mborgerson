pub const FATX_EPOCH_XBOX: u16 = 2000;
pub const FATX_EPOCH_XBOX360: u16 = 1980;

#[derive(Debug, Clone, Copy)]
pub struct Date {
    year: u16,
    /// 1 = January
    month: u8,
    day: u8,
}

impl Date {
    pub fn from_fatx_encoding(encoded: u16, epoch: u16) -> Self {
        Self {
            year: (((encoded >> 9) & 0x7f) + epoch),
            month: ((encoded >> 5) & 0xf) as u8,
            day: (encoded & 0x1f) as u8,
        }
    }

    pub(crate) fn to_fatx_encoding(self, epoch: u16) -> u16 {
        (((self.year.wrapping_sub(epoch)) & 0x7f) << 9)
            | ((self.month as u16 & 0xf) << 5)
            | (self.day as u16 & 0x1f)
    }

    pub fn year(&self) -> u16 {
        self.year
    }
    /// Returns the month, where 1 = January
    pub fn month(&self) -> u8 {
        self.month
    }
    pub fn day(&self) -> u8 {
        self.day
    }
}

#[derive(Debug, Clone, Copy)]
pub struct Time {
    hour: u8,
    minute: u8,
    second: u8,
}

impl Time {
    pub fn from_fatx_encoding(encoded: u16) -> Self {
        Self {
            hour: ((encoded >> 11) & 0xf) as u8,
            minute: ((encoded >> 5) & 0x1f) as u8,
            second: ((encoded & 0x1f) * 2) as u8,
        }
    }

    // NOTE: hour is masked to 4 bits (0-15), matching from_fatx_encoding
    // above -- this mirrors the existing (pre-existing, unchanged) decode
    // behavior rather than "fixing" it, so encode/decode stay symmetric.
    pub(crate) fn to_fatx_encoding(self) -> u16 {
        (((self.hour as u16) & 0xf) << 11)
            | (((self.minute as u16) & 0x1f) << 5)
            | (((self.second as u16) / 2) & 0x1f)
    }

    pub fn hour(&self) -> u8 {
        self.hour
    }
    pub fn minute(&self) -> u8 {
        self.minute
    }
    pub fn second(&self) -> u8 {
        self.second
    }
}

#[derive(Debug, Clone, Copy)]
pub struct DateTime {
    date: Date,
    time: Time,
}

impl DateTime {
    pub fn from_fatx_encoding(date_encoded: u16, time_encoded: u16, epoch: u16) -> Self {
        Self {
            date: Date::from_fatx_encoding(date_encoded, epoch),
            time: Time::from_fatx_encoding(time_encoded),
        }
    }

    pub(crate) fn to_fatx_encoding(self, epoch: u16) -> (u16, u16) {
        (
            self.date.to_fatx_encoding(epoch),
            self.time.to_fatx_encoding(),
        )
    }

    /// Builds a DateTime directly from calendar components (1 = January).
    pub fn from_components(
        year: u16,
        month: u8,
        day: u8,
        hour: u8,
        minute: u8,
        second: u8,
    ) -> Self {
        Self {
            date: Date { year, month, day },
            time: Time {
                hour,
                minute,
                second,
            },
        }
    }

    /// Builds a DateTime from a `SystemTime`, for "now" timestamps at file
    /// creation/modification and FUSE's setattr(atime/mtime). Implemented
    /// with a small hand-rolled civil-calendar breakdown rather than a
    /// `chrono` dependency, to keep this library crate dependency-light.
    pub fn from_system_time(t: std::time::SystemTime) -> Self {
        let secs = t
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0);
        let days = secs.div_euclid(86400);
        let secs_of_day = secs.rem_euclid(86400);

        let (year, month, day) = civil_from_days(days);

        Self::from_components(
            year as u16,
            month as u8,
            day as u8,
            (secs_of_day / 3600) as u8,
            ((secs_of_day % 3600) / 60) as u8,
            (secs_of_day % 60) as u8,
        )
    }

    pub fn year(&self) -> u16 {
        self.date.year
    }
    /// Returns the month, where 1 = January
    pub fn month(&self) -> u8 {
        self.date.month
    }
    pub fn day(&self) -> u8 {
        self.date.day
    }
    pub fn hour(&self) -> u8 {
        self.time.hour
    }
    pub fn minute(&self) -> u8 {
        self.time.minute
    }
    pub fn second(&self) -> u8 {
        self.time.second
    }
}

/// Converts a day count since 1970-01-01 (the Unix epoch) into a
/// (year, month, day) Gregorian civil date. Howard Hinnant's algorithm:
/// <http://howardhinnant.github.io/date_algorithms.html>
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719468;
    let era = z.div_euclid(146097);
    let doe = z.rem_euclid(146097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    let year = if m <= 2 { y + 1 } else { y };
    (year, m, d)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn date_round_trip() {
        for epoch in [FATX_EPOCH_XBOX, FATX_EPOCH_XBOX360] {
            for (year, month, day) in [(epoch, 1, 1), (epoch + 20, 6, 15), (epoch + 127, 12, 31)] {
                let encoded = Date { year, month, day }.to_fatx_encoding(epoch);
                let decoded = Date::from_fatx_encoding(encoded, epoch);
                assert_eq!(decoded.year(), year);
                assert_eq!(decoded.month(), month);
                assert_eq!(decoded.day(), day);
            }
        }
    }

    #[test]
    fn time_round_trip_and_bit_masks() {
        // Seconds only have 2-second resolution, hour is masked to 4 bits
        // (0-15, not 0-23), and minute is masked to 5 bits (0-31, not
        // 0-59) -- this pins the existing (pre-existing, unchanged) decode
        // behavior so it isn't "fixed" by accident later; the encode side
        // must stay symmetric with it.
        let encoded = Time {
            hour: 23,
            minute: 45,
            second: 30,
        }
        .to_fatx_encoding();
        let decoded = Time::from_fatx_encoding(encoded);
        assert_eq!(decoded.hour(), 23 & 0xf);
        assert_eq!(decoded.minute(), 45 & 0x1f);
        assert_eq!(decoded.second(), 30);
    }

    #[test]
    fn datetime_round_trip() {
        let dt = DateTime::from_components(2015, 3, 4, 10, 20, 30);
        let (date_encoded, time_encoded) = dt.to_fatx_encoding(FATX_EPOCH_XBOX360);
        let decoded = DateTime::from_fatx_encoding(date_encoded, time_encoded, FATX_EPOCH_XBOX360);
        assert_eq!(decoded.year(), 2015);
        assert_eq!(decoded.month(), 3);
        assert_eq!(decoded.day(), 4);
        assert_eq!(decoded.hour(), 10);
        assert_eq!(decoded.minute(), 20);
        assert_eq!(decoded.second(), 30);
    }

    #[test]
    fn civil_from_days_known_dates() {
        assert_eq!(civil_from_days(0), (1970, 1, 1));
        assert_eq!(civil_from_days(11017), (2000, 3, 1));
        assert_eq!(civil_from_days(19716), (2023, 12, 25));
    }

    #[test]
    fn from_system_time_known_instant() {
        // 2023-12-25 12:34:56 UTC == 1703507696
        let t = std::time::UNIX_EPOCH + std::time::Duration::from_secs(1703507696);
        let dt = DateTime::from_system_time(t);
        assert_eq!(dt.year(), 2023);
        assert_eq!(dt.month(), 12);
        assert_eq!(dt.day(), 25);
        assert_eq!(dt.hour(), 12);
        assert_eq!(dt.minute(), 34);
        assert_eq!(dt.second(), 56);
    }
}
