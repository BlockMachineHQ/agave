//! External consumers exercise the same delivery loop as native replay.
#![cfg(feature = "agave-unstable-api")]

use {
    agave_reserved_account_keys::ReservedAccountKeys,
    solana_entry::entry::EntryType,
    solana_hash::Hash,
    solana_keypair::Keypair,
    solana_ledger::blockstore_processor::{
        LockedTransactionsWithIndexes, ReplayBatchProcessor, ReplayEntry, ReplayEntryProcessor,
        process_entries_with_processor, schedule_batches_with_callbacks,
    },
    solana_message::SimpleAddressLoader,
    solana_pubkey::Pubkey,
    solana_runtime_transaction::runtime_transaction::RuntimeTransaction,
    solana_transaction::sanitized::{MessageHash, SanitizedTransaction},
    solana_transaction_error::{TransactionError, TransactionResult},
    std::{cell::RefCell, collections::VecDeque},
};

type Tx = RuntimeTransaction<SanitizedTransaction>;

#[derive(Debug, PartialEq)]
enum Event {
    Lock,
    Unlock(usize),
    Process(usize),
    Submit(std::ops::Range<usize>),
    Tick(Hash),
}

#[derive(Default)]
struct Processor {
    results: RefCell<VecDeque<Vec<TransactionResult<()>>>>,
    events: RefCell<Vec<Event>>,
    submit_error: Option<TransactionError>,
    tick_height: u64,
    max_tick_height: u64,
}

impl ReplayBatchProcessor for Processor {
    type Error = TransactionError;

    fn try_lock_accounts(&self, txs: &[Tx]) -> Vec<TransactionResult<()>> {
        self.events.borrow_mut().push(Event::Lock);
        let results = self.results.borrow_mut().pop_front().unwrap();
        assert_eq!(results.len(), txs.len());
        results
    }

    fn unlock_accounts(&self, txs: &[Tx], results: &[TransactionResult<()>]) {
        assert_eq!(txs.len(), results.len());
        self.events
            .borrow_mut()
            .push(Event::Unlock(results.iter().filter(|r| r.is_ok()).count()));
    }

    fn process_batches(
        &mut self,
        batches: impl ExactSizeIterator<Item = LockedTransactionsWithIndexes<SanitizedTransaction>>,
    ) -> TransactionResult<()> {
        self.events.borrow_mut().push(Event::Process(batches.len()));
        schedule_batches_with_callbacks(
            batches,
            |txs, results| self.unlock_accounts(txs, results),
            |txs, indexes| {
                assert_eq!(txs.len(), indexes.len());
                self.events.borrow_mut().push(Event::Submit(indexes));
                self.submit_error.clone().map_or(Ok(()), Err)
            },
        )
    }
}

impl ReplayEntryProcessor for Processor {
    fn tick_height(&self) -> u64 {
        self.tick_height
    }

    fn is_block_boundary(&self, height: u64) -> bool {
        height == self.max_tick_height
    }

    fn register_tick(&mut self, hash: &Hash) {
        self.tick_height += 1;
        self.events.borrow_mut().push(Event::Tick(*hash));
    }
}

fn entry(starting_index: usize, count: usize) -> ReplayEntry {
    ReplayEntry {
        starting_index,
        entry: EntryType::Transactions(
            (0..count)
                .map(|_| {
                    RuntimeTransaction::try_create(
                        solana_system_transaction::transfer(
                            &Keypair::new(),
                            &Pubkey::new_unique(),
                            1,
                            Hash::new_unique(),
                        )
                        .into(),
                        MessageHash::Compute,
                        None,
                        SimpleAddressLoader::Disabled,
                        &ReservedAccountKeys::empty_key_set(),
                        true,
                    )
                    .unwrap()
                })
                .collect(),
        ),
    }
}

#[test]
fn failed_admission_delivers_prefix_before_retry_and_unlocks_partial_success() {
    let mut p = Processor {
        results: RefCell::new(VecDeque::from([
            vec![Ok(())],
            vec![Ok(()), Err(TransactionError::AlreadyProcessed)],
            vec![Ok(()), Err(TransactionError::AlreadyProcessed)],
        ])),
        ..Processor::default()
    };
    assert_eq!(
        process_entries_with_processor(&mut p, [entry(17, 1), entry(18, 2)]),
        Err(TransactionError::AlreadyProcessed)
    );
    assert_eq!(
        *p.events.borrow(),
        [
            Event::Lock,
            Event::Lock,
            Event::Unlock(1),
            Event::Process(1),
            Event::Unlock(1),
            Event::Submit(17..18),
            Event::Lock,
            Event::Unlock(1)
        ]
    );
    assert!(p.results.borrow().is_empty());
}

#[test]
fn prefix_submission_failure_wins_without_retry_and_unlocks_remaining_batches() {
    let mut p = Processor {
        results: RefCell::new(VecDeque::from([
            vec![Ok(())],
            vec![Ok(())],
            vec![Err(TransactionError::AlreadyProcessed)],
        ])),
        submit_error: Some(TransactionError::BlockhashNotFound),
        ..Processor::default()
    };
    assert_eq!(
        process_entries_with_processor(&mut p, [entry(31, 1), entry(32, 1), entry(33, 1)]),
        Err(TransactionError::BlockhashNotFound)
    );
    assert_eq!(
        *p.events.borrow(),
        [
            Event::Lock,
            Event::Lock,
            Event::Lock,
            Event::Unlock(0),
            Event::Process(2),
            Event::Unlock(1),
            Event::Submit(31..32),
            Event::Unlock(1)
        ]
    );
}

#[test]
fn successful_retry_and_boundary_tick_follow_submission() {
    let hash = Hash::new_unique();
    let mut p = Processor {
        results: RefCell::new(VecDeque::from([
            vec![Ok(())],
            vec![Err(TransactionError::AccountInUse)],
            vec![Ok(())],
        ])),
        tick_height: 7,
        max_tick_height: 8,
        ..Processor::default()
    };
    process_entries_with_processor(
        &mut p,
        [
            entry(11, 1),
            entry(12, 1),
            ReplayEntry {
                entry: EntryType::Tick(hash),
                starting_index: 13,
            },
            entry(13, 1), // The native block boundary terminates this call's delivery.
        ],
    )
    .unwrap();
    assert_eq!(
        *p.events.borrow(),
        [
            Event::Lock,
            Event::Lock,
            Event::Unlock(0),
            Event::Process(1),
            Event::Unlock(1),
            Event::Submit(11..12),
            Event::Lock,
            Event::Process(1),
            Event::Unlock(1),
            Event::Submit(12..13),
            Event::Tick(hash)
        ]
    );
    assert_eq!(p.tick_height, 8);
}

#[test]
fn submission_error_prevents_tick_registration() {
    let mut p = Processor {
        results: RefCell::new(VecDeque::from([vec![Ok(())]])),
        submit_error: Some(TransactionError::BlockhashNotFound),
        max_tick_height: 1,
        ..Processor::default()
    };
    assert_eq!(
        process_entries_with_processor(
            &mut p,
            [
                entry(0, 1),
                ReplayEntry {
                    entry: EntryType::Tick(Hash::new_unique()),
                    starting_index: 1
                },
            ]
        ),
        Err(TransactionError::BlockhashNotFound)
    );
    assert_eq!(p.tick_height, 0);
    assert!(
        !p.events
            .borrow()
            .iter()
            .any(|event| matches!(event, Event::Tick(_)))
    );
}
