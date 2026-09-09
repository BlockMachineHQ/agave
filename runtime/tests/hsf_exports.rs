#![cfg(feature = "agave-unstable-api")]

use {
    solana_account::AccountSharedData,
    solana_epoch_schedule::EpochSchedule,
    solana_hash::Hash,
    solana_pubkey::Pubkey,
    solana_runtime::{
        consensus::{SlotHashKey, heaviest_subtree_fork_choice::HeaviestSubtreeForkChoice},
        epoch_stakes::{EpochStakes, VersionedEpochStakes},
        stakes::{SerdeStakesToStakeFormat, Stakes},
    },
    solana_vote::vote_account::{VoteAccount, VoteAccounts},
    solana_vote_interface::state::{VoteStateV4, VoteStateVersions},
    std::{cell::RefCell, collections::HashMap, sync::Arc},
};

fn key(slot: u64) -> SlotHashKey {
    (slot, Hash::default())
}

fn stakes(pubkey: Pubkey, stake: u64, epoch: u64) -> VersionedEpochStakes {
    let data = bincode::serialize(&VoteStateVersions::new_v4(VoteStateV4::default())).unwrap();
    let mut account = AccountSharedData::new(1, data.len(), &solana_sdk_ids::vote::id());
    account.set_data_from_slice(&data);
    let votes = VoteAccounts::from(Arc::new(HashMap::from([(
        pubkey,
        (stake, VoteAccount::try_from(account).unwrap()),
    )])));
    VersionedEpochStakes::Current {
        stakes: EpochStakes::from(SerdeStakesToStakeFormat::Stake(Stakes::new(votes, epoch))),
        total_stake: stake,
        node_id_to_vote_accounts: Arc::default(),
        epoch_authorized_voters: Arc::default(),
        bls_pubkey_to_rank_map: Default::default(),
    }
}

#[test]
fn map_and_lookup_cross_epochs_missing_stake_and_borrowed_owned_votes() {
    let schedule = EpochSchedule::custom(32, 32, false);
    let voter = Pubkey::new_unique();
    let absent = Pubkey::new_unique();
    let map = HashMap::from([(0, stakes(voter, 10, 0)), (1, stakes(voter, 25, 1))]);
    let mut native = HeaviestSubtreeForkChoice::new(key(0));
    for (slot, parent) in [(31, 0), (32, 0), (64, 32), (65, 64)] {
        native.add_new_leaf_slot(key(slot), Some(key(parent)));
    }
    let mut generic = native.clone();
    let calls = RefCell::new(Vec::new());
    for (slot, expected_calls, expected_stake) in [
        (31, vec![0], 10),
        (32, vec![0, 1], 25),
        (64, vec![1, 2], 0),
        (65, vec![2, 2], 0),
    ] {
        calls.borrow_mut().clear();
        let votes = [(voter, key(slot)), (absent, key(slot))];
        let expected = native.add_votes(votes.iter(), &map, &schedule);
        let actual =
            generic.add_votes_with_stake_lookup(votes.into_iter(), &schedule, |epoch, pk| {
                if pk == &voter {
                    calls.borrow_mut().push(epoch);
                }
                map.get(&epoch)
                    .map(|s| s.vote_account_stake(pk))
                    .unwrap_or(0)
            });
        assert_eq!(*calls.borrow(), expected_calls);
        assert_eq!(actual, expected);
        assert_eq!(generic.stake_voted_subtree(&key(0)), Some(expected_stake));
        for slot in [0, 31, 32, 64, 65] {
            assert_eq!(
                native.stake_voted_at(&key(slot)),
                generic.stake_voted_at(&key(slot))
            );
            assert_eq!(
                native.stake_voted_subtree(&key(slot)),
                generic.stake_voted_subtree(&key(slot))
            );
        }
    }
}

#[test]
fn lookup_ties_duplicate_hashes_pruning_and_validity() {
    let schedule = EpochSchedule::custom(32, 32, false);
    let voter = Pubkey::new_unique();
    let low = (1, Hash::new_from_array([1; 32]));
    let high = (1, Hash::new_from_array([2; 32]));
    let mut hsf = HeaviestSubtreeForkChoice::new(key(0));
    hsf.add_new_leaf_slot(high, Some(key(0)));
    hsf.add_new_leaf_slot(low, Some(key(0)));
    hsf.add_new_leaf_slot(key(2), Some(low));
    let mut native = hsf.clone();
    let map = HashMap::from([(0, stakes(voter, 10, 0))]);
    assert_eq!(hsf.best_overall_slot(), key(2));
    assert_eq!(native.best_overall_slot(), hsf.best_overall_slot());
    assert_eq!(
        native.add_votes([(voter, high)].into_iter(), &map, &schedule),
        high
    );
    assert_eq!(
        hsf.add_votes_with_stake_lookup([(voter, high)].iter(), &schedule, |_, _| 10),
        high
    );
    assert_eq!(
        hsf.add_votes_with_stake_lookup([(voter, low)].into_iter(), &schedule, |_, _| 10),
        key(2)
    );
    assert_eq!(
        native.add_votes([(voter, low)].iter(), &map, &schedule),
        hsf.best_overall_slot()
    );
    native.add_votes([(voter, high)].iter(), &map, &schedule);
    hsf.add_votes_with_stake_lookup([(voter, high)].into_iter(), &schedule, |_, _| {
        panic!("ignored vote")
    });
    assert_eq!(hsf.stake_voted_at(&high), Some(0));
    assert_eq!(hsf.stake_voted_at(&low), Some(10));
    assert_eq!(native.stake_voted_at(&high), hsf.stake_voted_at(&high));
    assert_eq!(native.stake_voted_at(&low), hsf.stake_voted_at(&low));
    hsf.mark_fork_invalid_candidate(&low);
    native.mark_fork_invalid_candidate(&low);
    assert_eq!(hsf.best_overall_slot(), high);
    assert_eq!(native.best_overall_slot(), hsf.best_overall_slot());
    assert_eq!(hsf.latest_invalid_ancestor(&key(2)), Some(1));
    assert_eq!(hsf.mark_fork_valid_candidate(&key(2)), vec![key(2), low]);
    assert_eq!(native.mark_fork_valid_candidate(&key(2)), vec![key(2), low]);
    assert_eq!(hsf.best_overall_slot(), key(2));
    assert_eq!(native.best_overall_slot(), hsf.best_overall_slot());
    assert_eq!(hsf.ancestors(key(2)), vec![low, key(0)]);
    hsf.set_tree_root(low);
    native.set_tree_root(low);
    assert!(!hsf.contains_block(&high));
    assert!(!native.contains_block(&high));
    assert_eq!(hsf.ancestors(key(2)), vec![low]);
    hsf.add_votes_with_stake_lookup([(voter, key(0))].into_iter(), &schedule, |_, _| {
        panic!("below root")
    });
    assert_eq!(hsf.stake_voted_subtree(&low), Some(10));
    assert_eq!(
        native.stake_voted_subtree(&low),
        hsf.stake_voted_subtree(&low)
    );
}

#[test]
#[should_panic(expected = "Should not get multiple votes for same pubkey in the same batch")]
fn lookup_rejects_duplicate_pubkeys() {
    let mut hsf = HeaviestSubtreeForkChoice::new(key(0));
    let voter = Pubkey::new_unique();
    hsf.add_votes_with_stake_lookup(
        [(voter, key(0)), (voter, key(0))].into_iter(),
        &EpochSchedule::default(),
        |_, _| 1,
    );
}
