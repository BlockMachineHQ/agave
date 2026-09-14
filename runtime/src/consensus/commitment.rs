//! Synchronous Tower commitment aggregation shared by replay consumers and the
//! validator commitment service. This computes commitment; it does not select a
//! BankForks root. The local Tower root and supermajority root remain distinct.

use {
    super::tower_vote_state::TowerVoteState,
    crate::{
        bank::Bank,
        commitment::{BlockCommitment, BlockCommitmentCache, CommitmentSlots, VOTE_THRESHOLD_SIZE},
    },
    solana_clock::Slot,
    solana_pubkey::Pubkey,
    std::collections::HashMap,
};

pub fn get_highest_super_majority_root(
    mut rooted_stake: Vec<(Slot, u64)>,
    total_stake: u64,
) -> Slot {
    rooted_stake.sort_by(|a, b| a.0.cmp(&b.0).reverse());
    let mut stake_sum = 0;
    for (root, stake) in rooted_stake {
        stake_sum += stake;
        if (stake_sum as f64 / total_stake as f64) > VOTE_THRESHOLD_SIZE {
            return root;
        }
    }
    0
}

pub fn aggregate_commitment(
    ancestors: &[Slot],
    bank: &Bank,
    (node_vote_pubkey, node_vote_state): &(Pubkey, TowerVoteState),
) -> (HashMap<Slot, BlockCommitment>, Vec<(Slot, u64)>) {
    assert!(!ancestors.is_empty());
    for a in ancestors.windows(2) {
        assert!(a[0] < a[1]);
    }

    let mut commitment = HashMap::new();
    let mut rooted_stake = Vec::new();
    for (pubkey, (lamports, account)) in bank.vote_accounts().iter() {
        if *lamports == 0 {
            continue;
        }
        let vote_state = if pubkey == node_vote_pubkey {
            // The production service overrides this node's landed vote with
            // its latest local Tower state, including for staked callers.
            node_vote_state.clone()
        } else {
            TowerVoteState::from(account.vote_state_view())
        };
        aggregate_commitment_for_vote_account(
            &mut commitment,
            &mut rooted_stake,
            &vote_state,
            ancestors,
            *lamports,
        );
    }
    (commitment, rooted_stake)
}

pub fn aggregate_commitment_for_vote_account(
    commitment: &mut HashMap<Slot, BlockCommitment>,
    rooted_stake: &mut Vec<(Slot, u64)>,
    vote_state: &TowerVoteState,
    ancestors: &[Slot],
    lamports: u64,
) {
    assert!(!ancestors.is_empty());
    let mut ancestors_index = 0;
    if let Some(root) = vote_state.root_slot {
        for (i, a) in ancestors.iter().enumerate() {
            if *a <= root {
                commitment
                    .entry(*a)
                    .or_default()
                    .increase_rooted_stake(lamports);
            } else {
                ancestors_index = i;
                break;
            }
        }
        rooted_stake.push((root, lamports));
    }

    for vote in &vote_state.votes {
        while ancestors[ancestors_index] <= vote.slot() {
            commitment
                .entry(ancestors[ancestors_index])
                .or_default()
                .increase_confirmation_stake(vote.confirmation_count() as usize, lamports);
            ancestors_index += 1;
            if ancestors_index == ancestors.len() {
                return;
            }
        }
    }
}

/// Build the production commitment update before the service takes its write
/// lock. Monotonic supermajority-root merging belongs at publication, after the
/// lock is acquired, so concurrent updates cannot move the published root back.
pub fn calculate_commitment_cache(
    ancestors: &[Slot],
    bank: &Bank,
    root: Slot,
    total_stake: u64,
    node_vote_state: &(Pubkey, TowerVoteState),
) -> BlockCommitmentCache {
    let (block_commitment, rooted_stake) = aggregate_commitment(ancestors, bank, node_vote_state);
    let highest_super_majority_root = get_highest_super_majority_root(rooted_stake, total_stake);
    let mut cache = BlockCommitmentCache::new(
        block_commitment,
        total_stake,
        CommitmentSlots {
            slot: bank.slot(),
            root,
            highest_confirmed_slot: root,
            highest_super_majority_root,
        },
    );
    let highest_confirmed_slot = cache.calculate_highest_confirmed_slot();
    cache.set_highest_confirmed_slot(highest_confirmed_slot);
    cache
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn supermajority_is_strict_and_counts_unrooted_stake_in_denominator() {
        assert_eq!(get_highest_super_majority_root(vec![(9, 2)], 3), 0);
        assert_eq!(get_highest_super_majority_root(vec![(9, 2), (5, 1)], 3), 5);
        assert_eq!(get_highest_super_majority_root(vec![(9, 2), (5, 1)], 5), 0);
        assert_eq!(get_highest_super_majority_root(vec![], 0), 0);
    }

    #[test]
    fn aggregation_keeps_landed_root_separate_from_vote_confirmations() {
        let mut votes = TowerVoteState::default();
        votes.root_slot = Some(2);
        votes.process_next_vote_slot(4);
        votes.process_next_vote_slot(6);
        let mut commitment = HashMap::new();
        let mut roots = vec![];
        aggregate_commitment_for_vote_account(
            &mut commitment,
            &mut roots,
            &votes,
            &[1, 2, 4, 6, 8],
            7,
        );
        assert_eq!(roots, vec![(2, 7)]);
        assert_eq!(commitment[&1].get_rooted_stake(), 7);
        assert_eq!(commitment[&2].get_rooted_stake(), 7);
        assert_eq!(commitment.get_mut(&4).unwrap().get_confirmation_stake(2), 7);
        assert_eq!(commitment.get_mut(&6).unwrap().get_confirmation_stake(1), 7);
        assert!(!commitment.contains_key(&8));
    }
}
