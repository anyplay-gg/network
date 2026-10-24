//! Orders the answers to a question the net service asks the cloud again and again, such as the
//! friend lists. Answers can arrive in another order than they were asked; one that arrives late
//! must never undo a newer one.

/// The questions asked and the newest answer taken.
#[derive(Debug, Default)]
pub struct Answers {
    asked: u64,
    taken: u64,
}

impl Answers {
    /// A new question. Its answer is checked with [`Answers::take`].
    pub fn ask(&mut self) -> u64 {
        self.asked += 1;
        self.asked
    }

    /// True when the answer to `question` is newer than every answer taken so far; it is then
    /// the newest.
    pub fn take(&mut self, question: u64) -> bool {
        if question > self.taken {
            self.taken = question;
            true
        } else {
            false
        }
    }

    /// News the cloud pushed: newer than every answer still on its way.
    pub fn pushed(&mut self) {
        let question = self.ask();
        self.take(question);
    }

    /// Every answer still on its way is ignored, as after a sign-out.
    pub fn ignore_pending(&mut self) {
        self.taken = self.asked;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_late_answer_never_undoes_a_newer_one() {
        let mut answers = Answers::default();
        let first = answers.ask();
        let second = answers.ask();
        assert!(answers.take(second));
        assert!(!answers.take(first));
        // In order, both count.
        let third = answers.ask();
        let fourth = answers.ask();
        assert!(answers.take(third));
        assert!(answers.take(fourth));
    }

    #[test]
    fn a_push_is_newer_than_every_answer_on_its_way() {
        let mut answers = Answers::default();
        let before = answers.ask();
        answers.pushed();
        assert!(!answers.take(before));
        let after = answers.ask();
        assert!(answers.take(after));
    }

    #[test]
    fn answers_on_their_way_can_be_ignored() {
        let mut answers = Answers::default();
        let pending = answers.ask();
        answers.ignore_pending();
        assert!(!answers.take(pending));
        let next = answers.ask();
        assert!(answers.take(next));
    }
}
