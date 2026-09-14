//! Native per-slot optimistic/duplicate confirmation accounting. Evidence
//! verification, slot admission and notification ordering belong to the caller.
//! This is shared with the production cluster vote listener, not a second oracle.

use {
    super::vote_stake_tracker::VoteStakeTracker, crate::commitment::VOTE_THRESHOLD_SIZE,
    solana_hash::Hash, solana_pubkey::Pubkey, std::collections::HashMap,
};

pub const SWITCH_FORK_THRESHOLD: f64 = 0.38;
pub const DUPLICATE_LIVENESS_THRESHOLD: f64 = 0.1;
pub const DUPLICATE_THRESHOLD: f64 = 1.0 - SWITCH_FORK_THRESHOLD - DUPLICATE_LIVENESS_THRESHOLD;
const THRESHOLDS_TO_CHECK: [f64; 2] = [DUPLICATE_THRESHOLD, VOTE_THRESHOLD_SIZE];
const MAX_VOTE_HASHES_PER_PUBKEY_PER_SLOT: u8 = 2;

#[derive(Default)]
pub struct SlotConfirmationTracker {
    optimistic_votes_tracker: HashMap<Hash, VoteStakeTracker>,
    num_optimistic_vote_hashes: HashMap<Pubkey, u8>,
}

impl SlotConfirmationTracker {
    /// Returns threshold crossings in duplicate/optimistic order and whether
    /// this voter was new for this hash. The two-hash cap is checked first, as in
    /// the production listener, even for a repeated vote on an existing hash.
    pub fn add_vote(
        &mut self,
        hash: Hash,
        pubkey: Pubkey,
        stake: u64,
        total_epoch_stake: u64,
    ) -> (Vec<bool>, bool) {
        let num_vote_hashes = self.num_optimistic_vote_hashes.entry(pubkey).or_default();
        if *num_vote_hashes >= MAX_VOTE_HASHES_PER_PUBKEY_PER_SLOT {
            return (vec![false; THRESHOLDS_TO_CHECK.len()], false);
        }
        let result @ (_, is_new) = self
            .optimistic_votes_tracker
            .entry(hash)
            .or_default()
            .add_vote_pubkey(pubkey, stake, total_epoch_stake, &THRESHOLDS_TO_CHECK);
        if is_new {
            *num_vote_hashes += 1;
        }
        result
    }

    pub fn votes(&self, hash: &Hash) -> Option<&VoteStakeTracker> {
        self.optimistic_votes_tracker.get(hash)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn distinct_thresholds_cross_once() {
        let mut tracker = SlotConfirmationTracker::default();
        let hash = Hash::new_unique();
        let voter = Pubkey::new_unique();
        assert_eq!(
            tracker.add_vote(hash, voter, 52, 100),
            (vec![false, false], true)
        );
        assert_eq!(
            tracker.add_vote(hash, voter, 52, 100),
            (vec![false, false], false)
        );
        assert_eq!(
            tracker.add_vote(hash, Pubkey::new_unique(), 1, 100),
            (vec![true, false], true)
        );
        assert_eq!(
            tracker.add_vote(hash, Pubkey::new_unique(), 13, 100),
            (vec![false, false], true)
        );
        assert_eq!(
            tracker.add_vote(hash, Pubkey::new_unique(), 1, 100),
            (vec![false, true], true)
        );
        assert_eq!(tracker.votes(&hash).unwrap().stake(), 67);
    }

    #[test]
    fn equivocator_can_attest_two_hashes_but_not_three() {
        let mut tracker = SlotConfirmationTracker::default();
        let voter = Pubkey::new_unique();
        let hashes = [Hash::new_unique(), Hash::new_unique(), Hash::new_unique()];
        for hash in &hashes[..2] {
            assert_eq!(
                tracker.add_vote(*hash, voter, 100, 100),
                (vec![true, true], true)
            );
        }
        assert_eq!(
            tracker.add_vote(hashes[2], voter, 100, 100),
            (vec![false, false], false)
        );
        assert!(tracker.votes(&hashes[2]).is_none());
        assert_eq!(
            tracker.add_vote(hashes[0], voter, 100, 100),
            (vec![false, false], false)
        );
        assert_eq!(tracker.votes(&hashes[0]).unwrap().stake(), 100);
    }
}
