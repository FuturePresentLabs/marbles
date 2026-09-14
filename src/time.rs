//! Business-hours claim deadlines.
//!
//! The whole point is that a human's 48 business hours started Friday 16:00 must not expire
//! Sunday 16:00. The math walks a weekly calendar accumulating working time, and it runs at
//! claim time only: the deadline stored is a plain UTC timestamp, so the sweeper stays a dumb
//! `expires_at < now` compare and a held deadline can always be explained after the fact.
//!
//! v0.1 uses fixed UTC offsets rather than a tz database — right for a single-owner fleet, and
//! the seam for real calendars later is this module alone.

use chrono::{DateTime, Datelike, Duration, FixedOffset, TimeZone, Utc};

use crate::types::Policy;

/// A week's worth of working time, repeated. Days are ISO (1 = Monday).
#[derive(Debug, Clone, PartialEq)]
pub struct WorkWeek {
    pub days: Vec<u32>,
    pub start_hour: u32,
    pub end_hour: u32,
    pub utc_offset_minutes: i32,
}

impl Default for WorkWeek {
    fn default() -> Self {
        Self {
            days: vec![1, 2, 3, 4, 5],
            start_hour: 9,
            end_hour: 17,
            utc_offset_minutes: 0,
        }
    }
}

impl WorkWeek {
    fn timezone(&self) -> FixedOffset {
        FixedOffset::east_opt(self.utc_offset_minutes * 60).expect("offset in range")
    }

    /// The earliest working instant at or after `t`.
    fn working_instant_or_after(&self, t: DateTime<FixedOffset>) -> DateTime<FixedOffset> {
        let mut day = t.date_naive();
        loop {
            if self.days.contains(&day.weekday().number_from_monday()) {
                let start = day
                    .and_hms_opt(self.start_hour, 0, 0)
                    .expect("configured start hour");
                let end = self.end_of(day);
                let naive = t.naive_local();
                if naive < start {
                    return t
                        .timezone()
                        .from_local_datetime(&start)
                        .single()
                        .expect("fixed offset, no DST");
                }
                if naive < end {
                    return t;
                }
            }
            day = day.succ_opt().expect("calendar overflow");
        }
    }

    /// The end of `t`'s working day; `t` must be inside one.
    /// The day's closing instant; `end_hour: 24` is round-the-clock and means midnight after.
    fn end_of(&self, day: chrono::NaiveDate) -> chrono::NaiveDateTime {
        if self.end_hour == 24 {
            day.succ_opt()
                .expect("calendar overflow")
                .and_hms_opt(0, 0, 0)
                .expect("midnight")
        } else {
            day.and_hms_opt(self.end_hour, 0, 0)
                .expect("configured end hour")
        }
    }

    fn day_end(&self, t: &DateTime<FixedOffset>) -> DateTime<FixedOffset> {
        let end = self.end_of(t.date_naive());
        t.timezone()
            .from_local_datetime(&end)
            .single()
            .expect("fixed offset, no DST")
    }

    /// The wall-clock instant `minutes` of working time after `now`.
    pub fn deadline(&self, now: &DateTime<Utc>, minutes: i64) -> DateTime<Utc> {
        let mut cursor = self.working_instant_or_after(now.with_timezone(&self.timezone()));
        let mut remaining = minutes;
        loop {
            let end = self.day_end(&cursor);
            let available = (end - cursor).num_minutes();
            if remaining <= available {
                return (cursor + Duration::minutes(remaining)).with_timezone(&Utc);
            }
            remaining -= available;
            cursor = self.working_instant_or_after(end + Duration::minutes(1));
        }
    }

    /// The stored expiry for a human claim made at `now`.
    pub fn human_expiry(&self, now: &DateTime<Utc>, policy: &Policy) -> i64 {
        self.deadline(now, policy.human_ttl_business_hours * 60)
            .timestamp()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn utc(y: i32, mo: u32, d: u32, h: u32, mi: u32) -> DateTime<Utc> {
        Utc.with_ymd_and_hms(y, mo, d, h, mi, 0).unwrap()
    }

    fn week() -> WorkWeek {
        WorkWeek {
            utc_offset_minutes: 0,
            ..Default::default()
        }
    }

    #[test]
    fn a_friday_afternoon_claim_does_not_expire_on_sunday() {
        // 2026-09-11 is a Friday. 48 business hours at 8/day spans Fri(1) Mon–Fri(40) Mon(7).
        let fri_16 = utc(2026, 9, 11, 16, 0);
        let expired = week().human_expiry(&fri_16, &Policy::default());
        assert_eq!(
            Utc.timestamp_opt(expired, 0).unwrap(),
            utc(2026, 9, 21, 16, 0)
        );
    }

    #[test]
    fn a_weekend_claim_starts_counting_on_monday() {
        // 2026-09-12 is a Saturday; 48 business hours ends Monday 17:00 of the following week.
        let sat = utc(2026, 9, 12, 12, 0);
        let expired = week().human_expiry(&sat, &Policy::default());
        // Mon 9 → 48h = 6 whole days: Mon..Fri (40) + Mon 8h.
        assert_eq!(
            Utc.timestamp_opt(expired, 0).unwrap(),
            utc(2026, 9, 21, 17, 0)
        );
    }

    #[test]
    fn one_business_hour_at_night_lands_next_morning() {
        let tue_22 = utc(2026, 9, 15, 22, 0);
        let expired = week().human_expiry(
            &tue_22,
            &Policy {
                human_ttl_business_hours: 1,
                ..Default::default()
            },
        );
        assert_eq!(
            Utc.timestamp_opt(expired, 0).unwrap(),
            utc(2026, 9, 16, 10, 0)
        );
    }

    #[test]
    fn working_minutes_within_a_day_are_not_deferred() {
        let tue_10 = utc(2026, 9, 15, 10, 0);
        let expired = week().deadline(&tue_10, 60);
        assert_eq!(expired, utc(2026, 9, 15, 11, 0));
    }

    #[test]
    fn the_deadline_never_moves_backwards() {
        let sat = utc(2026, 9, 12, 12, 0);
        assert!(week().deadline(&sat, 1).timestamp() > sat.timestamp());
    }
}
