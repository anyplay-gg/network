//! The pace of typing signals. The launcher may send one on every key; the cloud hears one per
//! conversation every [`anyplay_api::TYPING_RESEND_SECS`] seconds, and a "stopped" always.

use std::collections::HashMap;
use std::time::{Duration, Instant};

#[derive(Debug, Default)]
pub struct TypingThrottle {
    sent: HashMap<String, Instant>,
}

impl TypingThrottle {
    /// True when a "typing" signal for the conversation goes to the cloud now. Entries older
    /// than the pause are forgotten on the way, so the map stays as small as the conversations
    /// typed in lately.
    pub fn allow(&mut self, conversation: &str, now: Instant) -> bool {
        let pause = Duration::from_secs(anyplay_api::TYPING_RESEND_SECS);
        self.sent
            .retain(|_, sent| now.duration_since(*sent) < pause);
        if self.sent.contains_key(conversation) {
            return false;
        }
        self.sent.insert(conversation.to_owned(), now);
        true
    }

    /// The player stopped: the next "typing" goes through at once.
    pub fn stop(&mut self, conversation: &str) {
        self.sent.remove(conversation);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn one_signal_per_pause_and_conversation() {
        let mut throttle = TypingThrottle::default();
        let start = Instant::now();
        assert!(throttle.allow("a", start));
        assert!(!throttle.allow("a", start + Duration::from_millis(900)));
        assert!(throttle.allow("b", start + Duration::from_millis(900)));
        assert!(!throttle.allow("a", start + Duration::from_millis(2999)));
        assert!(throttle.allow("a", start + Duration::from_secs(3)));
    }

    #[test]
    fn stopping_lets_the_next_signal_through() {
        let mut throttle = TypingThrottle::default();
        let start = Instant::now();
        assert!(throttle.allow("a", start));
        throttle.stop("a");
        assert!(throttle.allow("a", start + Duration::from_millis(100)));
    }

    #[test]
    fn old_entries_do_not_pile_up() {
        let mut throttle = TypingThrottle::default();
        let start = Instant::now();
        for k in 0..100 {
            assert!(throttle.allow(&format!("c{k}"), start));
        }
        assert!(throttle.allow("late", start + Duration::from_secs(10)));
        assert_eq!(throttle.sent.len(), 1);
    }
}
