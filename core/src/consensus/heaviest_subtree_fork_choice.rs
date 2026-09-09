pub use solana_runtime::consensus::{
    SlotHashKey,
    heaviest_subtree_fork_choice::{ForkWeight, GetSlotHash, HeaviestSubtreeForkChoice},
};
use {
    crate::consensus::{
        Tower, fork_choice::ForkChoice,
        latest_validator_votes_for_frozen_banks::LatestValidatorVotesForFrozenBanks,
        progress_map::ProgressMap,
    },
    solana_measure::measure::Measure,
    solana_runtime::{bank::Bank, bank_forks::BankForks},
    std::{
        collections::{HashMap, HashSet},
        sync::{Arc, RwLock},
    },
};

trait HeaviestVotedFork {
    fn heaviest_slot_on_same_voted_fork(&self, tower: &Tower) -> Option<SlotHashKey>;
}

impl HeaviestVotedFork for HeaviestSubtreeForkChoice {
    fn heaviest_slot_on_same_voted_fork(&self, tower: &Tower) -> Option<SlotHashKey> {
        tower
            .last_voted_slot_hash()
            .and_then(|last_voted_slot_hash| {
                match self.is_candidate(&last_voted_slot_hash) {
                    Some(true) => self.best_slot(&last_voted_slot_hash),
                    Some(false) => {
                        // In this case our last voted fork has been marked invalid because
                        // it contains a duplicate block. It is critical that we continue to
                        // build on it as long as there exists at least 1 non duplicate fork.
                        // This is because there is a chance that this fork is actually duplicate
                        // confirmed but not observed because there is no block containing the
                        // required votes.
                        //
                        // Scenario 1:
                        // Slot 0 - Slot 1 (90%)
                        //        |
                        //        - Slot 1'
                        //        |
                        //        - Slot 2 (10%)
                        //
                        // Imagine that 90% of validators voted for Slot 1, but because of the existence
                        // of Slot 1', Slot 1 is marked as invalid in fork choice. It is impossible to reach
                        // the required switch threshold for these validators to switch off of Slot 1 to Slot 2.
                        // In this case it is important for someone to build a Slot 3 off of Slot 1 that contains
                        // the votes for Slot 1. At this point they will see that the fork off of Slot 1 is duplicate
                        // confirmed, and the rest of the network can repair Slot 1, and mark it is a valid candidate
                        // allowing fork choice to converge.
                        //
                        // This will only occur after Slot 2 has been created, in order to resolve the following
                        // scenario:
                        //
                        // Scenario 2:
                        // Slot 0 - Slot 1 (30%)
                        //        |
                        //        - Slot 1' (30%)
                        //
                        // In this scenario only 60% of the network has voted before the duplicate proof for Slot 1 and 1'
                        // was viewed. Neither version of the slot will reach the duplicate confirmed threshold, so it is
                        // critical that a new fork Slot 2 from Slot 0 is created to allow the validators on Slot 1 and
                        // Slot 1' to switch. Since the `best_slot` is an ancestor of the last vote (Slot 0 is ancestor of last
                        // vote Slot 1 or Slot 1'), we will trigger `SwitchForkDecision::FailedSwitchDuplicateRollback`, which
                        // will create an alternate fork off of Slot 0. Once this alternate fork is created, the `best_slot`
                        // will be Slot 2, at which point we will be in Scenario 1 and continue building off of Slot 1 or Slot 1'.
                        //
                        // For more details see the case for
                        // `SwitchForkDecision::FailedSwitchDuplicateRollback` in `ReplayStage::select_vote_and_reset_forks`.
                        self.deepest_slot(&last_voted_slot_hash)
                    }
                    None => {
                        if !tower.is_stray_last_vote() {
                            // Unless last vote is stray and stale, self.is_candidate(last_voted_slot_hash) must return
                            // Some(_), justifying to panic! here.
                            // Also, adjust_lockouts_after_replay() correctly makes last_voted_slot None,
                            // if all saved votes are ancestors of replayed_root_slot. So this code shouldn't be
                            // touched in that case as well.
                            // In other words, except being stray, all other slots have been voted on while this
                            // validator has been running, so we must be able to fetch best_slots for all of
                            // them.
                            panic!(
                                "a bank at last_voted_slot({last_voted_slot_hash:?}) is a frozen \
                                 bank so must have been added to heaviest_subtree_fork_choice at \
                                 time of freezing",
                            )
                        } else {
                            // fork_infos doesn't have corresponding data for the stale stray last vote,
                            // meaning some inconsistency between saved tower and ledger.
                            // (newer snapshot, or only a saved tower is moved over to new setup?)
                            None
                        }
                    }
                }
            })
    }
}

impl ForkChoice for HeaviestSubtreeForkChoice {
    type ForkChoiceKey = SlotHashKey;
    fn compute_bank_stats(
        &mut self,
        bank: &Bank,
        _tower: &Tower,
        latest_validator_votes_for_frozen_banks: &mut LatestValidatorVotesForFrozenBanks,
    ) {
        let mut start = Measure::start("compute_bank_stats_time");
        // Update `heaviest_subtree_fork_choice` to find the best fork to build on
        let root = self.tree_root().0;
        let new_votes = latest_validator_votes_for_frozen_banks.take_votes_dirty_set(root);
        let (best_overall_slot, best_overall_hash) =
            self.add_votes(new_votes, bank.epoch_stakes_map(), bank.epoch_schedule());
        start.stop();

        datapoint_info!(
            "compute_bank_stats-best_slot",
            ("computed_slot", bank.slot(), i64),
            ("overall_best_slot", best_overall_slot, i64),
            ("overall_best_hash", best_overall_hash.to_string(), String),
            ("elapsed", start.as_us(), i64),
        );
    }

    // Returns:
    // 1) The heaviest overall bank
    // 2) The heaviest bank on the same fork as the last vote (doesn't require a
    // switching proof to vote for)
    fn select_forks(
        &self,
        _frozen_banks: &[Arc<Bank>],
        tower: &Tower,
        _progress: &ProgressMap,
        _ancestors: &HashMap<u64, HashSet<u64>>,
        bank_forks: &RwLock<BankForks>,
    ) -> (Arc<Bank>, Option<Arc<Bank>>) {
        let r_bank_forks = bank_forks.read().unwrap();

        // BankForks should only contain one valid version of this slot
        (
            r_bank_forks
                .get_with_checked_hash(self.best_overall_slot())
                .unwrap(),
            self.heaviest_slot_on_same_voted_fork(tower)
                .and_then(|slot_hash| {
                    #[allow(clippy::manual_filter)]
                    if let Some(bank) = r_bank_forks.get(slot_hash.0) {
                        if bank.hash() != slot_hash.1 {
                            // It is possible that our last vote was for an invalid fork
                            // and we have repaired and replayed the correct version of the fork.
                            // In this case the hash for the heaviest bank on our voted fork
                            // will no longer be matching what we have replayed.
                            //
                            // Because we have dumped and repaired a new version, it is impossible
                            // for our last voted fork to become duplicate confirmed as the state
                            // machine will never dump and repair a block that has not been observed
                            // as duplicate confirmed. Therefore it is safe to never build on this
                            // invalid fork.
                            None
                        } else {
                            Some(bank)
                        }
                    } else {
                        // It is possible that our last vote was for an invalid fork
                        // and we are in the middle of dumping and repairing such fork.
                        // In that case, the `heaviest_slot_on_same_voted_fork` has a chance to
                        // be for a slot that we currently do not have in our bank forks, so we
                        // return None.
                        //
                        // We are guaranteed that we will eventually repair a duplicate confirmed version
                        // of this slot because the state machine will never dump a slot unless it has
                        // observed a duplicate confirmed version of the slot.
                        //
                        // Therefore there is no chance that our last voted fork will ever become
                        // duplicate confirmed, so it is safe to never build on it.
                        None
                    }
                }),
        )
    }

    fn mark_fork_invalid_candidate(&mut self, key: &SlotHashKey) {
        HeaviestSubtreeForkChoice::mark_fork_invalid_candidate(self, key);
    }

    fn mark_fork_valid_candidate(&mut self, key: &SlotHashKey) -> Vec<SlotHashKey> {
        HeaviestSubtreeForkChoice::mark_fork_valid_candidate(self, key)
    }
}

#[cfg(test)]
mod test {
    use {
        super::*, solana_hash::Hash, solana_pubkey::Pubkey, solana_runtime::bank_utils,
        solana_slot_history::SlotHistory, trees::tr,
    };
    #[test]
    fn test_runtime_type_identity_and_native_adapters() {
        fn assert_fork_choice<T: ForkChoice<ForkChoiceKey = SlotHashKey>>() {}
        assert_fork_choice::<
            solana_runtime::consensus::heaviest_subtree_fork_choice::HeaviestSubtreeForkChoice,
        >();
        let (bank, _) = bank_utils::setup_bank_and_vote_pubkeys_for_tests(1, 100);
        bank.freeze();
        let bank_forks = BankForks::new_rw_arc(bank);
        let root = bank_forks.read().unwrap().root_bank();
        let runtime: solana_runtime::consensus::heaviest_subtree_fork_choice::HeaviestSubtreeForkChoice =
            HeaviestSubtreeForkChoice::new_from_bank_forks(bank_forks.clone());
        let mut core: HeaviestSubtreeForkChoice = runtime;
        let key: solana_runtime::consensus::SlotHashKey = core.tree_root();
        assert_eq!(key, (root.slot(), root.hash()));
        let mut tower = Tower::new_for_tests(10, 0.9);
        let mut latest = LatestValidatorVotesForFrozenBanks::default();
        ForkChoice::compute_bank_stats(&mut core, &root, &tower, &mut latest);
        let (best, same) = core.select_forks(
            &[],
            &tower,
            &ProgressMap::default(),
            &HashMap::new(),
            &bank_forks,
        );
        assert_eq!(best.hash(), root.hash());
        assert!(same.is_none());

        // A dumped/repaired last-voted bank may be absent or have a different hash.
        let missing = (1, Hash::new_unique());
        core.add_new_leaf_slot(missing, Some(key));
        ForkChoice::mark_fork_invalid_candidate(&mut core, &missing);
        tower.record_vote(missing.0, missing.1);
        let (_, same) = core.select_forks(
            &[],
            &tower,
            &ProgressMap::default(),
            &HashMap::new(),
            &bank_forks,
        );
        assert!(same.is_none());
        let repaired = Bank::new_from_parent(root, Default::default(), 1);
        repaired.freeze();
        bank_forks.write().unwrap().insert(repaired);
        let (_, same) = core.select_forks(
            &[],
            &tower,
            &ProgressMap::default(),
            &HashMap::new(),
            &bank_forks,
        );
        assert!(same.is_none());
        assert_eq!(
            ForkChoice::mark_fork_valid_candidate(&mut core, &missing),
            vec![missing]
        );
    }

    #[test]
    #[should_panic(expected = "a bank at last_voted_slot")]
    fn test_missing_non_stray_last_vote_panics() {
        let hsf = HeaviestSubtreeForkChoice::new((0, Hash::default()));
        let mut tower = Tower::new_for_tests(10, 0.9);
        tower.record_vote(1, Hash::default());
        hsf.heaviest_slot_on_same_voted_fork(&tower);
    }

    #[test]
    fn test_stray_restored_slot() {
        let forks = tr(0) / (tr(1) / tr(2));
        let heaviest_subtree_fork_choice = HeaviestSubtreeForkChoice::new_from_tree(forks);

        let mut tower = Tower::new_for_tests(10, 0.9);
        tower.record_vote(1, Hash::default());

        assert!(!tower.is_stray_last_vote());
        assert_eq!(
            heaviest_subtree_fork_choice.heaviest_slot_on_same_voted_fork(&tower),
            Some((2, Hash::default()))
        );

        // Make slot 1 (existing in bank_forks) a restored stray slot
        let mut slot_history = SlotHistory::default();
        slot_history.add(0);
        // Work around TooOldSlotHistory
        slot_history.add(999);
        tower = tower
            .adjust_lockouts_after_replay(0, &slot_history)
            .unwrap();

        assert!(tower.is_stray_last_vote());
        assert_eq!(
            heaviest_subtree_fork_choice.heaviest_slot_on_same_voted_fork(&tower),
            Some((2, Hash::default()))
        );

        // Make slot 3 (NOT existing in bank_forks) a restored stray slot
        tower.record_vote(3, Hash::default());
        tower = tower
            .adjust_lockouts_after_replay(0, &slot_history)
            .unwrap();

        assert!(tower.is_stray_last_vote());
        assert_eq!(
            heaviest_subtree_fork_choice.heaviest_slot_on_same_voted_fork(&tower),
            None
        );
    }

    #[test]
    fn test_mark_valid_invalid_forks() {
        let mut heaviest_subtree_fork_choice = setup_forks();
        let stake = 100;
        let (bank, vote_pubkeys) = bank_utils::setup_bank_and_vote_pubkeys_for_tests(3, stake);

        let pubkey_votes: Vec<(Pubkey, SlotHashKey)> = vec![
            (vote_pubkeys[0], (6, Hash::default())),
            (vote_pubkeys[1], (6, Hash::default())),
            (vote_pubkeys[2], (2, Hash::default())),
        ];
        let expected_best_slot = 6;
        assert_eq!(
            heaviest_subtree_fork_choice.add_votes(
                pubkey_votes.iter(),
                bank.epoch_stakes_map(),
                bank.epoch_schedule()
            ),
            (expected_best_slot, Hash::default()),
        );
        assert_eq!(
            heaviest_subtree_fork_choice.deepest_overall_slot(),
            (expected_best_slot, Hash::default()),
        );

        // Simulate a vote on slot 5
        let last_voted_slot_hash = (5, Hash::default());
        let mut tower = Tower::new_for_tests(10, 0.9);
        tower.record_vote(last_voted_slot_hash.0, last_voted_slot_hash.1);

        // The heaviest_slot_on_same_voted_fork() should be 6, descended from 5.
        assert_eq!(
            heaviest_subtree_fork_choice
                .heaviest_slot_on_same_voted_fork(&tower)
                .unwrap(),
            (6, Hash::default())
        );

        // Mark slot 5 as invalid
        let invalid_candidate = last_voted_slot_hash;
        heaviest_subtree_fork_choice.mark_fork_invalid_candidate(&invalid_candidate);
        assert!(
            !heaviest_subtree_fork_choice
                .is_candidate(&invalid_candidate)
                .unwrap()
        );

        // The ancestor 3 is still a candidate
        assert!(
            heaviest_subtree_fork_choice
                .is_candidate(&(3, Hash::default()))
                .unwrap()
        );

        // The best fork should be its ancestor 3, not the other fork at 4.
        assert_eq!(heaviest_subtree_fork_choice.best_overall_slot().0, 3);

        // After marking the last vote in the tower as invalid, `heaviest_slot_on_same_voted_fork()`
        // should instead use the deepest slot metric, which is still 6
        assert_eq!(
            heaviest_subtree_fork_choice.heaviest_slot_on_same_voted_fork(&tower),
            Some((6, Hash::default()))
        );

        // Adding another descendant to the invalid candidate won't
        // update the best slot, even if it contains votes
        let new_leaf7 = (7, Hash::default());
        heaviest_subtree_fork_choice.add_new_leaf_slot(new_leaf7, Some((6, Hash::default())));
        let invalid_slot_ancestor = 3;
        assert_eq!(
            heaviest_subtree_fork_choice.best_overall_slot().0,
            invalid_slot_ancestor
        );
        let pubkey_votes: Vec<(Pubkey, SlotHashKey)> = vec![(vote_pubkeys[0], new_leaf7)];
        assert_eq!(
            heaviest_subtree_fork_choice.add_votes(
                pubkey_votes.iter(),
                bank.epoch_stakes_map(),
                bank.epoch_schedule()
            ),
            (invalid_slot_ancestor, Hash::default()),
        );

        // However this should update the `heaviest_slot_on_same_voted_fork` since we use
        // deepest metric for invalid forks
        assert_eq!(
            heaviest_subtree_fork_choice
                .heaviest_slot_on_same_voted_fork(&tower)
                .unwrap(),
            new_leaf7,
        );

        // Adding a descendant to the ancestor of the invalid candidate *should* update
        // the best slot though, since the ancestor is on the heaviest fork
        let new_leaf8 = (8, Hash::default());
        heaviest_subtree_fork_choice
            .add_new_leaf_slot(new_leaf8, Some((invalid_slot_ancestor, Hash::default())));
        assert_eq!(heaviest_subtree_fork_choice.best_overall_slot(), new_leaf8,);
        // Should not update the `heaviest_slot_on_same_voted_fork` because the new leaf
        // is not descended from the last vote
        assert_eq!(
            heaviest_subtree_fork_choice
                .heaviest_slot_on_same_voted_fork(&tower)
                .unwrap(),
            new_leaf7
        );

        // If we mark slot a descendant of `invalid_candidate` as valid, then that
        // should also mark `invalid_candidate` as valid, and the best slot should
        // be the leaf of the heaviest fork, `new_leaf_slot`.
        heaviest_subtree_fork_choice.mark_fork_valid_candidate(&invalid_candidate);
        assert!(
            heaviest_subtree_fork_choice
                .is_candidate(&invalid_candidate)
                .unwrap()
        );
        assert_eq!(
            heaviest_subtree_fork_choice.best_overall_slot(),
            // Should pick the smaller slot of the two new equally weighted leaves
            new_leaf7
        );
        // Should update the `heaviest_slot_on_same_voted_fork` as well
        assert_eq!(
            heaviest_subtree_fork_choice
                .heaviest_slot_on_same_voted_fork(&tower)
                .unwrap(),
            new_leaf7
        );
    }

    fn setup_forks() -> HeaviestSubtreeForkChoice {
        /*
            Build fork structure:
                 slot 0
                   |
                 slot 1
                 /    \
            slot 2    |
               |    slot 3
            slot 4    |
                    slot 5
                      |
                    slot 6
        */
        let forks = tr(0) / (tr(1) / (tr(2) / (tr(4))) / (tr(3) / (tr(5) / (tr(6)))));
        HeaviestSubtreeForkChoice::new_from_tree(forks)
    }
}
