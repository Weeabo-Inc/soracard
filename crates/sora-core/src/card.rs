//! Card-presence detection policy.
//!
//! The goal that the Realtek driver fails at: notice a card *arrive* and
//! *leave* promptly and tell `PnP`. Raw presence it not always clean, so we
//! debounce: a transition is only reported after `debounce` consecutive
//! samples agree, and `Unknown` samples never flip the stable state.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Presence {
    Absent,
    Present,
    /// Sampled but inconclusive (e.g. device busy, command timed out).
    Unknown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Event {
    Arrived,
    Removed,
}

pub struct Detector {
    stable: Presence,
    candidate: Presence,
    count: u8,
    debounce: u8,
}

impl Detector {
    /// `debounce` of 1 means "trust every sample". Clamped to at least 1.
    #[must_use]
    pub const fn new(initial: Presence, debounce: u8) -> Self {
        let debounce = if debounce == 0 { 1 } else { debounce };
        Self {
            stable: initial,
            candidate: initial,
            count: 0,
            debounce,
        }
    }

    #[must_use]
    pub const fn stable(&self) -> Presence {
        self.stable
    }

    /// Feed one raw sample. Returns the debounced transition, if any.
    pub fn sample(&mut self, raw: Presence) -> Option<Event> {
        if raw == self.candidate {
            self.count = self.count.saturating_add(1);
        } else {
            self.candidate = raw;
            self.count = 1;
        }

        if self.count < self.debounce || raw == self.stable {
            return None;
        }

        let event = match (self.stable, raw) {
            (_, Presence::Present) => Event::Arrived,
            (_, Presence::Absent) => Event::Removed,
            // Never report a transition into Unknown; keep the last known state.
            (_, Presence::Unknown) => return None,
        };
        self.stable = raw;
        self.count = 0;
        Some(event)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn debounce_requires_consistency() {
        let mut d = Detector::new(Presence::Absent, 3);
        assert_eq!(d.sample(Presence::Present), None);
        assert_eq!(d.sample(Presence::Present), None);
        assert_eq!(d.sample(Presence::Present), Some(Event::Arrived));
        assert_eq!(d.stable(), Presence::Present);
    }

    #[test]
    fn noise_does_not_flap() {
        let mut d = Detector::new(Presence::Absent, 3);
        assert_eq!(d.sample(Presence::Present), None);
        assert_eq!(d.sample(Presence::Absent), None); // resets candidate
        assert_eq!(d.sample(Presence::Present), None);
        assert_eq!(d.sample(Presence::Present), None);
        assert_eq!(d.sample(Presence::Present), Some(Event::Arrived));
    }

    #[test]
    fn unknown_never_reports() {
        let mut d = Detector::new(Presence::Present, 1);
        assert_eq!(d.sample(Presence::Unknown), None);
        assert_eq!(d.stable(), Presence::Present);
        assert_eq!(d.sample(Presence::Absent), Some(Event::Removed));
    }

    #[test]
    fn prompt_both_directions() {
        let mut d = Detector::new(Presence::Absent, 2);
        d.sample(Presence::Present);
        assert_eq!(d.sample(Presence::Present), Some(Event::Arrived));
        d.sample(Presence::Absent);
        assert_eq!(d.sample(Presence::Absent), Some(Event::Removed));
    }
}
