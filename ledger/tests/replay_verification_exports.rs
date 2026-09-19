//! Exercise the storage-independent verification surface as an external caller.
//! No Bank or production dev-context feature is needed by this adapter.
#![cfg(feature = "agave-unstable-api")]

use {
    agave_reserved_account_keys::ReservedAccountKeys,
    rayon::ThreadPoolBuilder,
    solana_entry::entry::{
        Entry, entries_to_verification_data, validate_and_hash_transactions, verify_entries_cpu,
    },
    solana_hash::Hash,
    solana_keypair::Keypair,
    solana_ledger::{
        block_error::BlockError,
        blockstore_processor::{
            AsyncVerificationProgress, AsyncVerificationResult, BlockstoreProcessorError,
            TickVerificationContext, verify_ticks_with_context,
        },
    },
    solana_message::{SimpleAddressLoader, VersionedMessage},
    solana_pubkey::Pubkey,
    solana_runtime_transaction::runtime_transaction::RuntimeTransaction,
    solana_signature::Signature,
    solana_transaction::{sanitized::MessageHash, sanitized::SanitizedTransaction},
    solana_transaction_error::TransactionError,
};

#[test]
fn owned_verification_jobs_preserve_native_errors() {
    let verify_pool = ThreadPoolBuilder::new().num_threads(1).build().unwrap();
    let sanitize_pool = ThreadPoolBuilder::new().num_threads(1).build().unwrap();
    for failure in [None, Some("poh"), Some("signature")] {
        let start = Hash::new_unique();
        let mut tx =
            solana_system_transaction::transfer(&Keypair::new(), &Pubkey::new_unique(), 1, start);
        if failure == Some("signature") {
            tx.signatures[0] = Signature::default();
        }
        // Recompute PoH after signature corruption to isolate signature failure.
        let mut transaction_entry = Entry::new(&start, 1, vec![tx]);
        if failure == Some("poh") {
            transaction_entry.hash = Hash::new_unique();
        }
        let entries = vec![
            transaction_entry.clone(),
            Entry::new(&transaction_entry.hash, 3, vec![]),
        ];
        verify_ticks_with_context(
            TickVerificationContext {
                slot: 1,
                tick_height: 1,
                max_tick_height: 2,
                hashes_per_tick: Some(4),
                alpenglow_ticks: false,
            },
            &entries,
            true,
            &mut 0,
        )
        .unwrap();
        let data = entries_to_verification_data(&entries);
        let mut progress = AsyncVerificationProgress::new();
        let mut poh_us = 0;
        let mut signature_us = 0;
        progress
            .spawn(&verify_pool, &mut poh_us, &mut signature_us, move || {
                let state = verify_entries_cpu(&data, &start);
                AsyncVerificationResult {
                    poh_verify_elapsed: state.poh_duration_us(),
                    transaction_verify_elapsed: 0,
                    error: (!state.status()).then_some(BlockstoreProcessorError::InvalidBlock(
                        BlockError::InvalidEntryHash,
                    )),
                }
            })
            .unwrap();
        // Consumes raw entries while the owned PoH job may still be using them.
        let prepared = validate_and_hash_transactions(
            entries,
            1,
            &sanitize_pool,
            |tx, bytes| -> Result<RuntimeTransaction<SanitizedTransaction>, TransactionError> {
                RuntimeTransaction::try_create(
                    tx,
                    MessageHash::Precomputed(VersionedMessage::hash_raw_message(bytes)),
                    None,
                    SimpleAddressLoader::Disabled,
                    &ReservedAccountKeys::empty_key_set(),
                    true,
                )
            },
        )
        .unwrap();
        progress
            .spawn(&verify_pool, &mut poh_us, &mut signature_us, move || {
                AsyncVerificationResult {
                    poh_verify_elapsed: 0,
                    transaction_verify_elapsed: 0,
                    error: prepared
                        .unverified_signatures
                        .verify()
                        .map_err(BlockstoreProcessorError::from)
                        .err(),
                }
            })
            .unwrap();
        let result = progress.wait_for_all_results(&mut poh_us, &mut signature_us);
        match failure {
            None => result.unwrap(),
            Some("poh") => assert!(matches!(
                result,
                Err(BlockstoreProcessorError::InvalidBlock(
                    BlockError::InvalidEntryHash
                ))
            )),
            Some("signature") => assert!(matches!(
                result,
                Err(BlockstoreProcessorError::InvalidTransaction(
                    TransactionError::SignatureFailure
                ))
            )),
            _ => unreachable!(),
        }
        progress
            .collect_available_results(&mut poh_us, &mut signature_us)
            .unwrap();
    }
}
