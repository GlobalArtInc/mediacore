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
#[path = "history_tests.rs"]
mod tests;
