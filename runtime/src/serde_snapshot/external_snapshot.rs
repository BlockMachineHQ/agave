//! Native snapshot decoding and reconstruction over an independently verified image.
use {
    super::*,
    crate::snapshot_bank_utils::{
        restore_snapshot_status_cache, verify_bank_against_expected_slot_hash, verify_epoch_stakes,
        verify_snapshot_capitalization,
    },
    agave_snapshots::snapshot_hash::SnapshotHash,
    solana_accounts_db::{
        external_backend::{
            BankIdentity, ExternalAccountBackend, VerifiedSnapshotImage, terminal_backend_result,
        },
        partitioned_rewards::PartitionedEpochRewardsConfig,
    },
    std::io::{Cursor, Seek},
};

#[derive(Debug, thiserror::Error)]
pub enum ExternalSnapshotError {
    #[error("native snapshot decode: {0}")]
    Decode(#[from] bincode::Error),
    #[error("native snapshot verification: {0}")]
    Snapshot(#[from] SnapshotError),
    #[error("invalid snapshot attachment: {0}")]
    Invalid(&'static str),
    #[error("snapshot stakes inconsistent with account image: {0}")]
    Stakes(String),
}

/// Archive identities supplied by acquisition, independently of the bank streams.
/// An incremental stream must have been acquired for this full snapshot base.
/// Native streams do not encode a modern incremental base identity; acquisition
/// must validate the archive's base slot before invoking this entry point.
#[derive(Clone, Copy, Debug)]
pub struct SnapshotManifest {
    pub full: (Slot, SnapshotHash),
    pub incremental: Option<(Slot, SnapshotHash)>,
}

/// Restore a native frozen nonzero-slot Bank without constructing AccountsDb or
/// an accounts index. Streams are the native V1_2_0 bank files (not tar archives);
/// status bytes are the latest snapshot's native status_cache file. Bank readers
/// must be seekable and positioned at byte zero; each complete file is capped at
/// the native MAX_SNAPSHOT_DATA_FILE_SIZE and must be consumed exactly.
///
/// The caller independently verifies the merged account image and the provider
/// binds `image` to its actual contents. The full LT hash, capitalization and data
/// length are explicit: none are inferred from BM schemas. The native bank hash
/// and the archive snapshot hash are distinct and both are checked.
///
/// The provider must be a fresh parentless `(image.slot, 0)` view. Native view
/// consumption begins at `initialize_verified_snapshot`; earlier decode/input
/// failures do not initialize or consume it. A wrapper requiring one-shot attempts
/// must independently poison those earlier failures. Once initialization begins,
/// a failed attachment must not be retried or published.
/// The restored Bank already has freeze_started=true: native stores reject writes.
/// Feature/reward initialization reconstructs runtime caches; it does not perform
/// a new write/hash-completion phase. Sealing happens after native verification
/// succeeds. Backend faults remain terminal. Returning a Bank
/// does not root it or issue a durable checkpoint: publish through BankForks.
#[allow(clippy::too_many_arguments)]
pub fn bank_from_snapshot_streams_with_external_backend<R: Read + Seek>(
    streams: &mut SnapshotStreams<R>,
    status_cache_bytes: &[u8],
    manifest: SnapshotManifest,
    image: &VerifiedSnapshotImage,
    genesis_config: &GenesisConfig,
    runtime_config: &RuntimeConfig,
    backend: Arc<dyn ExternalAccountBackend>,
    rewards_config: PartitionedEpochRewardsConfig,
) -> Result<Bank, ExternalSnapshotError> {
    use ExternalSnapshotError::Invalid;
    let (fields, db_fields) = crate::snapshot_utils::deserialize_snapshot_streams_capped(
        streams,
        crate::snapshot_utils::MAX_SNAPSHOT_DATA_FILE_SIZE,
        |streams| fields_from_streams(streams).map_err(SnapshotError::from),
    )
    .map_err(|error| match error {
        SnapshotError::Serialize(error) => ExternalSnapshotError::Decode(error),
        error => ExternalSnapshotError::Snapshot(error),
    })?;
    verify_stream_identity(
        &fields.full,
        &db_fields.full_snapshot_accounts_db_fields,
        manifest.full,
    )?;
    match (
        &fields.incremental,
        &db_fields.incremental_snapshot_accounts_db_fields,
        manifest.incremental,
    ) {
        (Some(fields), Some(db_fields), Some(expected)) if expected.0 > manifest.full.0 => {
            verify_stream_identity(fields, db_fields, expected)?;
        }
        (None, None, None) => (),
        _ => {
            return Err(Invalid(
                "incremental stream/manifest mismatch or invalid slot order",
            ));
        }
    }
    let mut status_stream = BufReader::new(Cursor::new(status_cache_bytes));
    let slot_deltas = crate::snapshot_utils::deserialize_snapshot_streams_capped(
        &mut SnapshotStreams {
            full_snapshot_stream: &mut status_stream,
            incremental_snapshot_stream: None,
        },
        crate::snapshot_utils::MAX_SNAPSHOT_DATA_FILE_SIZE,
        |streams| status_cache::deserialize_status_cache_from(&mut *streams.full_snapshot_stream),
    )?;
    let mut fields = fields.collapse_into();
    fields.bank_hash_stats = db_fields.into_bank_hash_info().stats;
    if fields.slot == 0 || fields.hash == Hash::default() {
        return Err(Invalid(
            "external snapshot must be a frozen nonzero-slot bank",
        ));
    }
    if image.slot != fields.slot || image.bank_hash != fields.hash {
        return Err(Invalid("verified image bank slot/hash mismatch"));
    }
    if image.accounts_lt_hash != fields.accounts_lt_hash {
        return Err(Invalid("verified image accounts LT hash mismatch"));
    }
    if image.capitalization != fields.capitalization {
        return Err(SnapshotError::MismatchedCapitalization(
            fields.capitalization,
            image.capitalization,
        )
        .into());
    }
    if image.accounts_data_len != fields.accounts_data_len {
        return Err(Invalid("verified image accounts data length mismatch"));
    }
    if backend.identity()
        != (BankIdentity {
            slot: fields.slot,
            bank_id: 0,
        })
        || backend.parent_identity().is_some()
    {
        return Err(Invalid("snapshot backend must have BankId 0 and no parent"));
    }
    let epoch_stakes = reconstruct_epoch_stakes(std::mem::take(&mut fields.versioned_epoch_stakes));
    terminal_backend_result(backend.initialize_verified_snapshot(image));
    let bank = Bank::try_new_from_snapshot(
        BankRc::new(Accounts::new_external(
            backend.clone(),
            rewards_config.stake_account_stores_per_block,
        )),
        genesis_config,
        Arc::new(runtime_config.clone()),
        fields,
        None,
        None,
        image.accounts_data_len,
        epoch_stakes,
    )?;
    verify_snapshot_capitalization(&bank, image.capitalization, false)?;
    verify_epoch_stakes(&bank).map_err(SnapshotError::from)?;
    restore_snapshot_status_cache(&bank, &slot_deltas)?;
    let (slot, hash) = manifest.incremental.unwrap_or(manifest.full);
    verify_bank_against_expected_slot_hash(&bank, slot, hash)?;
    if !bank.verify_accounts(Some(&image.accounts_lt_hash)) || !bank.verify_hash() {
        return Err(Invalid(
            "snapshot accounts or frozen bank hash verification failed",
        ));
    }
    terminal_backend_result(backend.seal());
    Ok(bank)
}

fn verify_stream_identity<T>(
    fields: &BankFieldsToDeserialize,
    db_fields: &AccountsDbFields<T>,
    (slot, hash): (Slot, SnapshotHash),
) -> Result<(), ExternalSnapshotError> {
    if fields.slot != slot {
        return Err(SnapshotError::MismatchedSlot(fields.slot, slot).into());
    }
    if db_fields.2 != fields.slot {
        return Err(ExternalSnapshotError::Invalid(
            "bank/accounts metadata slot mismatch",
        ));
    }
    let actual_hash = SnapshotHash::new(fields.accounts_lt_hash.0.checksum());
    if actual_hash != hash {
        return Err(SnapshotError::MismatchedHash(actual_hash, hash).into());
    }
    Ok(())
}
