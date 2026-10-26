//! Monotonic event-id minting for one writer.
//!
//! A fresh ULID per call draws new random bits, so two ids minted in the same
//! millisecond sort in random order. The minter keeps the last id it issued and
//! increments it whenever the clock has not moved past that id's millisecond,
//! including when the clock steps backwards.

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use dreamd_protocol::EventId;
use ulid::Ulid;

/// A log id stamped more than this far ahead of the clock does not seed the
/// minter; seeding from it would stamp every new id that far ahead.
pub const MAX_SEED_SKEW: Duration = Duration::from_secs(24 * 60 * 60);

/// Issues strictly increasing ULIDs.
#[derive(Debug)]
pub struct IdMinter {
    last: Ulid,
}

impl IdMinter {
    /// A minter whose first id is above every qualifying id in `ids`.
    pub fn seeded<'a>(ids: impl IntoIterator<Item = &'a EventId>, now: SystemTime) -> Self {
        let mut minter = Self { last: Ulid::nil() };
        minter.observe(ids, now);
        minter
    }

    /// Raise the floor to the largest qualifying id in `ids`. Never lowers it.
    ///
    /// An id qualifies when its suffix is a ULID and its millisecond is at most
    /// [`MAX_SEED_SKEW`] past `now`. A ULID's first character is `0`–`7`;
    /// `Ulid::from_string` wraps a larger one instead of rejecting it.
    pub fn observe<'a>(&mut self, ids: impl IntoIterator<Item = &'a EventId>, now: SystemTime) {
        let cap_ms = unix_ms(now).saturating_add(MAX_SEED_SKEW.as_millis() as u64);
        let mut ahead = 0usize;
        for id in ids {
            let Some(suffix) = id.as_str().strip_prefix("evt_") else {
                continue;
            };
            if !matches!(suffix.as_bytes().first(), Some(b'0'..=b'7')) {
                continue;
            }
            let Ok(ulid) = Ulid::from_string(suffix) else {
                continue;
            };
            if ulid.timestamp_ms() > cap_ms {
                ahead += 1;
                continue;
            }
            self.last = self.last.max(ulid);
        }
        if ahead > 0 {
            tracing::warn!(
                ahead,
                "episodic log: ids more than 24 h ahead of the clock do not seed the id minter"
            );
        }
    }

    /// The clock's ULID when `now` is past the last id's millisecond, else the
    /// last id plus one.
    pub fn next_at(&mut self, now: SystemTime) -> EventId {
        let next = if unix_ms(now) > self.last.timestamp_ms() {
            Ulid::from_datetime(now)
        } else {
            self.last
                .increment()
                .unwrap_or_else(|| Ulid::from_parts(self.last.timestamp_ms() + 1, 0))
        };
        self.last = next;
        EventId::parse(&format!("evt_{next}")).expect("a ULID always parses as an EventId")
    }
}

fn unix_ms(t: SystemTime) -> u64 {
    t.duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as u64)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn t() -> SystemTime {
        UNIX_EPOCH + Duration::from_millis(1_780_000_000_000)
    }

    /// The millisecond a minted id carries.
    fn id_ms(id: &EventId) -> u64 {
        Ulid::from_string(&id.as_str()[4..])
            .expect("minted id is a ULID")
            .timestamp_ms()
    }

    fn evt(ulid: Ulid) -> EventId {
        EventId::parse(&format!("evt_{ulid}")).unwrap()
    }

    #[test]
    fn frozen_clock_mints_100k_strictly_increasing_ids() {
        let mut minter = IdMinter::seeded(std::iter::empty(), t());
        let mut prev = minter.next_at(t());
        for _ in 1..100_000 {
            let next = minter.next_at(t());
            assert!(next > prev, "{next} must sort above {prev}");
            prev = next;
        }
    }

    #[test]
    fn clock_stepping_backwards_keeps_ids_increasing() {
        let mut minter = IdMinter::seeded(std::iter::empty(), t());
        let a = minter.next_at(t());
        let b = minter.next_at(t() - Duration::from_secs(5));
        let c = minter.next_at(t() - Duration::from_secs(3600));
        assert!(a < b && b < c, "{a} < {b} < {c}");
        assert_eq!(id_ms(&b), unix_ms(t()));
        assert_eq!(id_ms(&c), unix_ms(t()));
    }

    #[test]
    fn clock_past_the_last_millisecond_uses_the_clock() {
        let mut minter = IdMinter::seeded(std::iter::empty(), t());
        let _ = minter.next_at(t());
        let second = minter.next_at(t() + Duration::from_millis(1));
        assert_eq!(id_ms(&second), unix_ms(t()) + 1);
    }

    #[test]
    fn exhausted_random_moves_to_the_next_millisecond() {
        let full = Ulid::from_parts(unix_ms(t()), (1 << 80) - 1);
        let seed = evt(full);
        let mut minter = IdMinter::seeded([&seed], t());
        let next = minter.next_at(t());
        assert!(next > seed, "{next} must sort above {seed}");
        assert_eq!(id_ms(&next), unix_ms(t()) + 1);
    }

    #[test]
    fn seed_is_the_largest_qualifying_log_id() {
        let older = evt(Ulid::from_parts(
            unix_ms(t() - Duration::from_secs(7200)),
            5,
        ));
        let newer = evt(Ulid::from_parts(
            unix_ms(t() - Duration::from_secs(3600)),
            5,
        ));
        for ids in [[&older, &newer], [&newer, &older]] {
            let mut minter = IdMinter::seeded(ids, t());
            let next = minter.next_at(t() - Duration::from_secs(3 * 3600));
            assert!(next > newer, "{next} must sort above {newer}");
        }
    }

    #[test]
    fn seed_skips_an_overflowing_first_character() {
        let near = Ulid::from_parts(unix_ms(t()) + 30 * 60 * 1000, 7).to_string();
        assert!(near.starts_with('0'), "precondition: {near} starts with 0");
        let twin = format!("evt_8{}", &near[1..]);
        let twin = EventId::parse(&twin).expect("precondition: the twin parses as an EventId");
        assert_eq!(
            Ulid::from_string(&twin.as_str()[4..])
                .expect("precondition: from_string accepts the twin")
                .to_string(),
            near,
            "precondition: from_string wraps the twin to `near`"
        );

        let old = evt(Ulid::from_parts(
            unix_ms(t() - Duration::from_secs(3600)),
            5,
        ));
        let mut minter = IdMinter::seeded([&old, &twin], t());
        assert_eq!(id_ms(&minter.next_at(t())), unix_ms(t()));
    }

    #[test]
    fn seed_skips_ids_more_than_a_day_ahead() {
        let ahead = evt(Ulid::from_parts(
            unix_ms(t() + Duration::from_secs(48 * 3600)),
            0,
        ));
        let mut minter = IdMinter::seeded([&ahead], t());
        assert_eq!(id_ms(&minter.next_at(t())), unix_ms(t()));
    }
}
