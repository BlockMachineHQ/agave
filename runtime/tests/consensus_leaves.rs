#![cfg(feature = "agave-unstable-api")]

use {
    solana_hash::Hash,
    solana_pubkey::Pubkey,
    solana_runtime::consensus::{
        SlotHashKey, latest_validator_votes_for_frozen_banks::LatestValidatorVotesForFrozenBanks,
        tower_vote_state::TowerVoteState, tree_diff::TreeDiff,
        unfrozen_gossip_verified_vote_hashes::UnfrozenGossipVerifiedVoteHashes,
        vote_stake_tracker::VoteStakeTracker,
    },
    solana_vote::vote_state_view::VoteStateView,
    solana_vote_program::vote_state::{
        Lockout, VoteState1_14_11, VoteStateV3, VoteStateV4, VoteStateVersions,
    },
    std::{collections::HashSet, sync::Arc},
};

#[test]
fn repeated_gossip_preserves_pending_duplicates_even_when_frozen() {
    let mut latest = LatestValidatorVotesForFrozenBanks::default();
    let mut pending = UnfrozenGossipVerifiedVoteHashes::default();
    let voter = Pubkey::new_unique();
    let hash = Hash::new_unique();

    for _ in 0..2 {
        pending.add_vote(voter, 10, hash, false, &mut latest);
    }
    assert_eq!(pending.remove_slot_hash(10, &hash), Some(vec![voter; 2]));
    assert!(pending.votes_per_slot[&10].is_empty());
    assert_eq!(pending.remove_slot_hash(10, &hash), None);

    pending.add_vote(voter, 10, hash, true, &mut latest);
    assert!(pending.votes_per_slot[&10].is_empty());
    // The original producer queues repeated frozen votes too; do not deduplicate here.
    for _ in 0..2 {
        pending.add_vote(voter, 10, hash, true, &mut latest);
    }
    assert_eq!(pending.votes_per_slot[&10][&hash], vec![voter; 2]);
    assert_eq!(latest.max_gossip_frozen_votes()[&voter], (10, vec![hash]));
    assert_eq!(latest.take_votes_dirty_set(0).count(), 0);

    pending.add_vote(voter, 9, hash, false, &mut latest);
    assert!(!pending.votes_per_slot.contains_key(&9));
    pending.add_vote(voter, 11, hash, false, &mut latest);
    pending.set_root(10);
    assert!(pending.votes_per_slot.contains_key(&10));
    pending.set_root(11);
    assert!(!pending.votes_per_slot.contains_key(&10));
    assert_eq!(pending.remove_slot_hash(11, &hash), Some(vec![voter]));
}

#[test]
fn replay_dirty_votes_are_independent_owned_and_drained_at_root() {
    let mut latest = LatestValidatorVotesForFrozenBanks::default();
    let voter = Pubkey::new_unique();
    let first = Hash::new_unique();
    let second = Hash::new_unique();
    assert_eq!(latest.check_add_vote(voter, 10, None, false), (false, None));
    assert_eq!(
        latest.check_add_vote(voter, 10, Some(first), false),
        (true, Some(10))
    );
    assert_eq!(latest.take_votes_dirty_set(0).count(), 0);
    assert_eq!(
        latest.check_add_vote(voter, 10, Some(first), true),
        (true, Some(10))
    );
    assert_eq!(
        latest.check_add_vote(voter, 10, Some(first), true),
        (false, Some(10))
    );
    assert_eq!(
        latest.check_add_vote(voter, 10, Some(second), true),
        (true, Some(10))
    );
    let dirty = latest.take_votes_dirty_set(10);
    assert_eq!(latest.take_votes_dirty_set(0).count(), 0);
    let actual: HashSet<(Pubkey, SlotHashKey)> = dirty.collect();
    assert_eq!(
        actual,
        HashSet::from([(voter, (10, first)), (voter, (10, second))])
    );
    assert_eq!(
        latest.check_add_vote(voter, 11, Some(first), true),
        (true, Some(11))
    );
    assert_eq!(latest.take_votes_dirty_set(12).count(), 0);
    assert_eq!(latest.take_votes_dirty_set(0).count(), 0);
    assert_eq!(latest.max_gossip_frozen_votes()[&voter], (10, vec![first]));
}

#[test]
fn stake_threshold_is_strict_and_each_pubkey_counts_once() {
    let mut tracker = VoteStakeTracker::default();
    let voter = Pubkey::new_unique();
    assert_eq!(
        tracker.add_vote_pubkey(voter, 52, 100, &[0.52]),
        (vec![false], true)
    );
    assert_eq!(
        tracker.add_vote_pubkey(voter, 100, 100, &[0.52]),
        (vec![false], false)
    );
    assert_eq!(tracker.stake(), 52);
    assert_eq!(tracker.voted(), &HashSet::from([voter]));
    assert_eq!(
        tracker.add_vote_pubkey(Pubkey::new_unique(), 1, 100, &[0.52]),
        (vec![true], true)
    );
    assert_eq!(tracker.stake(), 53);
}

#[test]
fn tower_owned_states_and_borrowed_serialized_views_agree() {
    let expected = TowerVoteState {
        votes: [Lockout::new_with_confirmation_count(8, 3), Lockout::new(10)].into(),
        root_slot: Some(5),
    };
    let legacy: VoteState1_14_11 = expected.clone().into();
    let v3 = VoteStateV3 {
        votes: expected.votes.iter().copied().map(Into::into).collect(),
        root_slot: expected.root_slot,
        ..VoteStateV3::default()
    };
    let v4 = VoteStateV4 {
        votes: v3.votes.clone(),
        root_slot: expected.root_slot,
        ..VoteStateV4::default()
    };
    assert_eq!(TowerVoteState::from(legacy.clone()), expected);
    assert_eq!(TowerVoteState::from(v3.clone()), expected);
    assert_eq!(TowerVoteState::from(v4.clone()), expected);
    for version in [
        VoteStateVersions::V1_14_11(Box::new(legacy)),
        VoteStateVersions::V3(Box::new(v3)),
        VoteStateVersions::V4(Box::new(v4)),
    ] {
        let view = VoteStateView::try_new(Arc::new(bincode::serialize(&version).unwrap())).unwrap();
        assert_eq!(TowerVoteState::from(&view), expected);
        assert_eq!(view.root_slot(), expected.root_slot);
    }
    assert_eq!(expected.tower(), vec![8, 10]);
    assert_eq!(expected.last_voted_slot(), Some(10));
    assert_eq!(expected.nth_recent_lockout(0), expected.last_lockout());
    assert_eq!(expected.nth_recent_lockout(usize::MAX), None);
}

#[test]
fn external_tree_diff_implementation_excludes_the_whole_subtree() {
    struct Tree([Vec<u64>; 4]);
    impl<'a> TreeDiff<'a> for &'a Tree {
        type TreeKey = u64;
        type ChildIter = std::slice::Iter<'a, u64>;

        fn children(&self, key: &u64) -> Option<Self::ChildIter> {
            self.0.get(*key as usize).map(|children| children.iter())
        }

        fn contains_slot(&self, slot: &u64) -> bool {
            self.0.get(*slot as usize).is_some()
        }
    }
    let tree = &Tree([vec![1, 2], vec![3], vec![], vec![]]);
    assert_eq!(tree.subtree_diff(0, 1), HashSet::from([0, 2]));
    assert_eq!(tree.subtree_diff(0, 0), HashSet::new());
    assert_eq!(tree.subtree_diff(9, 1), HashSet::new());
    assert_eq!(tree.subtree_diff(0, 9), HashSet::from([0, 1, 2, 3]));
}
