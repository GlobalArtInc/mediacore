use std::collections::VecDeque;

pub const WINDOW: u64 = 1024;

const WORDS: usize = (WINDOW / 64) as usize;
const DELTA_ENTRIES: usize = 32;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Slot {
    New,
    Seen,
    TooOld,
}

#[derive(Debug, Clone)]
pub struct SeqWindow {
    highest: u64,
    started: bool,
    words: [u64; WORDS],
}

impl SeqWindow {
    pub fn new() -> Self {
        Self {
            highest: 0,
            started: false,
            words: [0; WORDS],
        }
    }

    pub fn observe(&mut self, seq: u64) -> Slot {
        if !self.started {
            self.started = true;
            self.highest = seq;
            self.mark(seq, true);
            return Slot::New;
        }

        if seq > self.highest {
            if seq - self.highest >= WINDOW {
                self.words = [0; WORDS];
            } else {
                for stale in self.highest + 1..=seq {
                    self.mark(stale, false);
                }
            }

            self.highest = seq;
            self.mark(seq, true);
            return Slot::New;
        }

        if self.highest - seq >= WINDOW {
            return Slot::TooOld;
        }

        if self.is_set(seq) {
            return Slot::Seen;
        }

        self.mark(seq, true);
        Slot::New
    }

    fn position(seq: u64) -> (usize, u64) {
        let slot = (seq % WINDOW) as usize;
        (slot / 64, 1u64 << (slot % 64))
    }

    fn mark(&mut self, seq: u64, present: bool) {
        let (word, bit) = Self::position(seq);
        if present {
            self.words[word] |= bit;
        } else {
            self.words[word] &= !bit;
        }
    }

    fn is_set(&self, seq: u64) -> bool {
        let (word, bit) = Self::position(seq);
        self.words[word] & bit != 0
    }
}

impl Default for SeqWindow {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct DeltaEntry {
    from: u64,
    delta: i64,
}

#[derive(Debug, Clone)]
pub struct DeltaHistory {
    entries: VecDeque<DeltaEntry>,
}

impl DeltaHistory {
    pub fn anchored(from: u64, delta: i64) -> Self {
        let mut entries = VecDeque::with_capacity(DELTA_ENTRIES);
        entries.push_back(DeltaEntry { from, delta });
        Self { entries }
    }

    pub fn current(&self) -> i64 {
        self.entries.back().map(|entry| entry.delta).unwrap_or(0)
    }

    pub fn shift(&mut self, from: u64, by: i64) {
        let delta = self.current().saturating_add(by);

        match self.entries.back_mut() {
            Some(last) if last.from >= from => last.delta = delta,
            _ => {
                if self.entries.len() == DELTA_ENTRIES {
                    self.entries.pop_front();
                }
                self.entries.push_back(DeltaEntry { from, delta });
            }
        }
    }

    pub fn at(&self, seq: u64) -> Option<i64> {
        self.entries
            .iter()
            .rev()
            .find(|entry| entry.from <= seq)
            .map(|entry| entry.delta)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_fresh_sequence_number_is_new_and_then_seen() {
        let mut window = SeqWindow::new();

        assert_eq!(window.observe(10), Slot::New);
        assert_eq!(window.observe(10), Slot::Seen);
        assert_eq!(window.observe(11), Slot::New);
        assert_eq!(window.observe(11), Slot::Seen);
    }

    #[test]
    fn a_late_filler_inside_the_window_is_new_once() {
        let mut window = SeqWindow::new();
        window.observe(100);
        window.observe(104);

        assert_eq!(window.observe(101), Slot::New);
        assert_eq!(window.observe(101), Slot::Seen);
        assert_eq!(window.observe(104), Slot::Seen);
    }

    #[test]
    fn anything_older_than_the_window_is_refused() {
        let mut window = SeqWindow::new();
        window.observe(10_000);

        assert_eq!(window.observe(10_000 - WINDOW), Slot::TooOld);
        assert_eq!(window.observe(10_000 - WINDOW + 1), Slot::New);
    }

    #[test]
    fn a_jump_past_the_window_forgets_everything_behind_it() {
        let mut window = SeqWindow::new();
        window.observe(5);
        window.observe(5 + WINDOW * 2);

        assert_eq!(window.observe(5 + WINDOW * 2 - 1), Slot::New);
    }

    #[test]
    fn a_delta_change_only_applies_from_its_own_position() {
        let mut history = DeltaHistory::anchored(0, -5);
        history.shift(120, -1);

        assert_eq!(history.at(119), Some(-5));
        assert_eq!(history.at(120), Some(-6));
        assert_eq!(history.current(), -6);
        assert_eq!(history.at(0), Some(-5));
    }

    #[test]
    fn nothing_before_the_anchor_can_be_translated() {
        let history = DeltaHistory::anchored(500, 7);

        assert_eq!(history.at(500), Some(7));
        assert_eq!(history.at(501), Some(7));
        assert_eq!(
            history.at(499),
            None,
            "a packet from before the anchor has no place in the outgoing stream"
        );
    }

    #[test]
    fn the_oldest_delta_is_dropped_when_the_history_is_full() {
        let mut history = DeltaHistory::anchored(0, 0);
        for step in 1..=DELTA_ENTRIES as u64 {
            history.shift(step * 10, -1);
        }

        assert_eq!(history.at(0), None);
        assert_eq!(history.current(), -(DELTA_ENTRIES as i64));
    }
}
