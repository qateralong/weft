pub const REJECT_AFTER_MESSAGES: u64 = u64::MAX - (1 << 13);

const WORD_BITS: u64 = u64::BITS as u64;
const WORDS: usize = 32;
const WINDOW: u64 = (WORDS as u64 - 1) * WORD_BITS;

#[derive(Clone, Debug)]
pub struct ReplayWindow {
    top: u64,
    bitmap: [u64; WORDS],
}

impl Default for ReplayWindow {
    fn default() -> Self {
        Self { top: 0, bitmap: [0; WORDS] }
    }
}

impl ReplayWindow {
    pub fn accept(&mut self, counter: u64) -> bool {
        if counter >= REJECT_AFTER_MESSAGES {
            return false;
        }
        let counter = counter + 1;
        if counter + WINDOW < self.top {
            return false;
        }
        let index = counter / WORD_BITS;
        if counter > self.top {
            let current = self.top / WORD_BITS;
            let stale = (index - current).min(WORDS as u64);
            for i in 1..=stale {
                self.bitmap[((current + i) % WORDS as u64) as usize] = 0;
            }
            self.top = counter;
        }
        let word = &mut self.bitmap[(index % WORDS as u64) as usize];
        let bit = 1 << (counter % WORD_BITS);
        let seen = *word & bit != 0;
        *word |= bit;
        !seen
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn in_order_and_duplicates() {
        let mut w = ReplayWindow::default();
        assert!(w.accept(0));
        assert!(!w.accept(0));
        assert!(w.accept(1));
        assert!(w.accept(2));
        assert!(!w.accept(1));
    }

    #[test]
    fn out_of_order_within_window() {
        let mut w = ReplayWindow::default();
        assert!(w.accept(100));
        assert!(w.accept(50));
        assert!(w.accept(99));
        assert!(!w.accept(50));
        assert!(w.accept(3000));
        assert!(w.accept(3000 - WINDOW));
    }

    #[test]
    fn too_old() {
        let mut w = ReplayWindow::default();
        assert!(w.accept(5000));
        assert!(!w.accept(5000 - WINDOW - 1));
        assert!(w.accept(5000 - WINDOW));
    }

    #[test]
    fn big_jump_clears_bitmap() {
        let mut w = ReplayWindow::default();
        for counter in 0..64 {
            assert!(w.accept(counter));
        }
        assert!(w.accept(1_000_000));
        assert!(w.accept(1_000_000 - 1));
        assert!(!w.accept(63));
        for counter in 1_000_001..1_003_000 {
            assert!(w.accept(counter));
        }
        assert!(!w.accept(1_000_000));
    }

    #[test]
    fn rejects_exhausted_counters() {
        let mut w = ReplayWindow::default();
        assert!(w.accept(REJECT_AFTER_MESSAGES - 1));
        assert!(!w.accept(REJECT_AFTER_MESSAGES));
        assert!(!w.accept(u64::MAX));
    }
}
