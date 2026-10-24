//! Orders the answers to a question the net service asks the cloud again and again, such as the
//! friend lists. Answers can arrive in another order than they were asked; one that arrives late
//! must never undo a newer one. Objects the cloud numbers, sessions and presence, are ordered by
//! their revision instead ([`is_older`]), because several nodes of a cloud may tell of them.

/// True when a copy with revision `new` is older than the one with `known` and must be dropped.
/// Revision 0 comes from a cloud that does not count, and is never older.
pub fn is_older(new: u64, known: u64) -> bool {
    new != 0 && new < known
}

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
    fn an_older_revision_is_dropped_and_a_cloud_that_does_not_count_is_believed() {
        assert!(is_older(3, 4));
        assert!(!is_older(4, 4), "the same state again changes nothing");
        assert!(!is_older(5, 4));
        assert!(!is_older(0, 4), "0 means the cloud does not count");
        assert!(!is_older(1, 0));
    }

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
