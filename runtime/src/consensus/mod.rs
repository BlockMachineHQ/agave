pub mod heaviest_subtree_fork_choice;
pub mod latest_validator_votes_for_frozen_banks;
pub mod tower_vote_state;
pub mod tree_diff;
pub mod unfrozen_gossip_verified_vote_hashes;
pub mod vote_stake_tracker;

use {solana_clock::Slot, solana_hash::Hash};

pub type SlotHashKey = (Slot, Hash);
