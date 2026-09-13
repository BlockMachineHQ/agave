use {
    agave_reserved_account_keys::ReservedAccountKeys,
    solana_hash::Hash,
    solana_keypair::Keypair,
    solana_message::SimpleAddressLoader,
    solana_runtime_transaction::runtime_transaction::RuntimeTransaction,
    solana_signer::Signer,
    solana_system_transaction::transfer,
    solana_transaction::{
        Transaction,
        sanitized::{MessageHash, SanitizedTransaction},
        versioned::VersionedTransaction,
    },
    solana_transaction_error::TransactionResult,
};

// Shared with the original native signature benchmark. Distinct amounts produce
// distinct signed messages; construction is outside all measured regions.
pub fn signed_transfers(keypair: &Keypair, amounts: std::ops::Range<usize>) -> Vec<Transaction> {
    amounts
        .map(|lamports| transfer(keypair, &keypair.pubkey(), lamports as u64, Hash::default()))
        .collect()
}

pub fn validate_transaction(
    versioned_tx: VersionedTransaction,
    message_bytes: &[u8],
) -> TransactionResult<RuntimeTransaction<SanitizedTransaction>> {
    RuntimeTransaction::try_create(
        versioned_tx,
        MessageHash::Precomputed(solana_message::VersionedMessage::hash_raw_message(
            message_bytes,
        )),
        None,
        SimpleAddressLoader::Disabled,
        &ReservedAccountKeys::empty_key_set(),
        true,
    )
}
