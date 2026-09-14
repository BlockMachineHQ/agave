use {
    super::*,
    crate::{
        genesis_utils::{ValidatorVoteKeypairs, create_genesis_config_with_vote_accounts},
        serde_snapshot::{
            self, ExternalSnapshotError, ExtraFieldsToSerialize, SnapshotManifest, SnapshotStreams,
            bank_from_snapshot_streams_with_external_backend,
        },
        snapshot_bank_utils::{
            restore_snapshot_status_cache, verify_bank_against_expected_slot_hash,
            verify_snapshot_capitalization,
        },
        snapshot_utils::StorageAndNextAccountsFileId,
    },
    agave_fs::io_setup::IoSetupState,
    agave_snapshots::error::SnapshotError,
    solana_accounts_db::{
        account_storage::AccountStorageMap,
        accounts_db::AtomicAccountsFileId,
        external_backend::{
            BackendResult, BankIdentity, ExternalAccountBackend, VerifiedSnapshotImage,
        },
        partitioned_rewards::PartitionedEpochRewardsConfig,
    },
    solana_lattice_hash::lt_hash::LtHash,
    std::io::{BufReader, Cursor, Read, Seek, SeekFrom},
};

type ImageAccounts = HashMap<Pubkey, (AccountSharedData, Slot)>;

#[derive(Debug)]
struct SnapshotView {
    id: BankIdentity,
    parent: Option<BankIdentity>,
    accounts: Mutex<ImageAccounts>,
    initialized: AtomicBool,
    stores: AtomicU64,
    sealed: AtomicBool,
    rooted: AtomicBool,
}

impl SnapshotView {
    fn new(slot: Slot, accounts: ImageAccounts) -> Arc<Self> {
        Arc::new(Self {
            id: BankIdentity { slot, bank_id: 0 },
            parent: None,
            accounts: Mutex::new(accounts),
            initialized: AtomicBool::new(false),
            stores: AtomicU64::new(0),
            sealed: AtomicBool::new(false),
            rooted: AtomicBool::new(false),
        })
    }
}

impl ExternalAccountBackend for SnapshotView {
    fn identity(&self) -> BankIdentity {
        self.id
    }
    fn parent_identity(&self) -> Option<BankIdentity> {
        self.parent
    }
    fn initialize_empty_genesis(&self) -> BackendResult<()> {
        Err("snapshot only".into())
    }
    fn initialize_verified_snapshot(&self, image: &VerifiedSnapshotImage) -> BackendResult<()> {
        assert_eq!(self.id.slot, image.slot);
        assert!(!self.initialized.swap(true, Relaxed));
        let accounts = self.accounts.lock().unwrap();
        let stats = image_stats(image.slot, image.bank_hash, &accounts);
        assert_eq!(stats.accounts_lt_hash, image.accounts_lt_hash);
        assert_eq!(stats.capitalization, image.capitalization);
        assert_eq!(stats.accounts_data_len, image.accounts_data_len);
        Ok(())
    }
    fn load(&self, key: &Pubkey) -> BackendResult<Option<(AccountSharedData, Slot)>> {
        Ok(self
            .accounts
            .lock()
            .unwrap()
            .get(key)
            .cloned()
            .filter(|(a, _)| a.lamports() != 0))
    }
    fn store(&self, accounts: &[(Pubkey, AccountSharedData)]) -> BackendResult<()> {
        self.stores.fetch_add(1, Relaxed);
        assert!(self.initialized.load(Relaxed));
        assert!(!self.sealed.load(Relaxed));
        for (key, account) in accounts {
            self.accounts
                .lock()
                .unwrap()
                .insert(*key, (account.clone(), self.id.slot));
        }
        Ok(())
    }
    fn fork_child(&self, id: BankIdentity) -> BackendResult<Arc<dyn ExternalAccountBackend>> {
        assert!(self.sealed.load(Relaxed));
        Ok(Arc::new(Self {
            id,
            parent: Some(self.id),
            accounts: Mutex::new(self.accounts.lock().unwrap().clone()),
            initialized: AtomicBool::new(true),
            stores: AtomicU64::new(0),
            sealed: AtomicBool::new(false),
            rooted: AtomicBool::new(false),
        }))
    }
    fn seal(&self) -> BackendResult<()> {
        self.sealed.store(true, Relaxed);
        Ok(())
    }
    fn apply_root(&self, path: &[BankIdentity]) -> BackendResult<()> {
        assert!(self.sealed.load(Relaxed));
        assert_eq!(path.last(), Some(&self.id));
        self.rooted.store(true, Relaxed);
        Ok(())
    }
    fn mark_root(&self) -> BackendResult<()> {
        assert!(self.rooted.load(Relaxed));
        Ok(())
    }
    fn remove_unrooted(&self, _: &[BankIdentity]) -> BackendResult<()> {
        Ok(())
    }
    fn release_bank(&self) -> BackendResult<()> {
        Ok(())
    }
}

fn image_stats(slot: Slot, bank_hash: Hash, accounts: &ImageAccounts) -> VerifiedSnapshotImage {
    let mut image = VerifiedSnapshotImage {
        slot,
        bank_hash,
        accounts_lt_hash: AccountsLtHash(LtHash::identity()),
        capitalization: 0,
        accounts_data_len: 0,
    };
    for (key, (account, _)) in accounts {
        if account.lamports() != 0 {
            image
                .accounts_lt_hash
                .0
                .mix_in(&AccountsDb::lt_hash_account(account, key).0);
            image.capitalization += account.lamports();
            image.accounts_data_len += account.data().len() as u64;
        }
    }
    image
}

struct Fixture {
    genesis: GenesisConfig,
    mint: solana_keypair::Keypair,
    bank: Arc<Bank>,
    full: Vec<u8>,
    incremental: Option<Vec<u8>>,
    status: Vec<u8>,
    manifest: SnapshotManifest,
    accounts: ImageAccounts,
    image: VerifiedSnapshotImage,
}

fn serialize(bank: &Bank, base: Option<Slot>) -> Vec<u8> {
    serialize_with(bank, base, |_| {})
}

fn serialize_with(
    bank: &Bank,
    base: Option<Slot>,
    mutate: impl FnOnce(&mut BankFieldsToSerialize),
) -> Vec<u8> {
    let mut fields = bank.get_fields_to_serialize();
    mutate(&mut fields);
    let extra = ExtraFieldsToSerialize {
        lamports_per_signature: fields.fee_rate_governor.lamports_per_signature,
        unused_incremental_snapshot_persistence: None,
        unused_epoch_accounts_hash: None,
        versioned_epoch_stakes: std::mem::take(&mut fields.versioned_epoch_stakes),
        accounts_lt_hash: Some(fields.accounts_lt_hash.clone().into()),
        block_id: Some(fields.block_id),
    };
    let mut bytes = Vec::new();
    serde_snapshot::serialize_bank_snapshot_into(
        &mut bytes,
        fields,
        bank.get_bank_hash_stats(),
        &bank.get_snapshot_storages(base),
        extra,
    )
    .unwrap();
    bytes
}

fn add_bank_accounts(bank: &Bank, accounts: &mut ImageAccounts) {
    for (key, account) in bank.get_all_accounts_modified_since_parent() {
        accounts.insert(key, (account, bank.slot()));
    }
}

fn fixture(incremental: bool) -> Fixture {
    let validator = ValidatorVoteKeypairs::new_rand();
    let info =
        create_genesis_config_with_vote_accounts(10_000_000_000, &[validator], vec![1_000_000_000]);
    let genesis = info.genesis_config;
    let forks = BankForks::new_rw_arc(Bank::new_for_tests(&genesis));
    let genesis_bank = forks.read().unwrap().root_bank();
    genesis_bank.freeze();
    let mut accounts = HashMap::new();
    add_bank_accounts(&genesis_bank, &mut accounts);
    let leader = Bank::slot_leader_from_epoch_stakes(
        3,
        genesis_bank.epoch_schedule(),
        genesis_bank.epoch_stakes_map(),
    );
    let full_bank = Arc::new(Bank::new_from_parent(genesis_bank, leader, 3));
    full_bank
        .transfer(123, &info.mint_keypair, &Pubkey::new_unique())
        .unwrap();
    full_bank.freeze();
    full_bank.squash();
    Bank::calculate_and_set_block_id_for_dcou(&full_bank);
    add_bank_accounts(&full_bank, &mut accounts);
    full_bank.force_flush_accounts_cache();
    let full = serialize(&full_bank, None);
    let mut manifest = SnapshotManifest {
        full: (full_bank.slot(), full_bank.get_snapshot_hash()),
        incremental: None,
    };
    let (bank, incremental) = if incremental {
        let leader = Bank::slot_leader_from_epoch_stakes(
            7,
            full_bank.epoch_schedule(),
            full_bank.epoch_stakes_map(),
        );
        let bank = Arc::new(Bank::new_from_parent(full_bank, leader, 7));
        bank.transfer(234, &info.mint_keypair, &Pubkey::new_unique())
            .unwrap();
        bank.freeze();
        bank.squash();
        Bank::calculate_and_set_block_id_for_dcou(&bank);
        add_bank_accounts(&bank, &mut accounts);
        bank.force_flush_accounts_cache();
        manifest.incremental = Some((bank.slot(), bank.get_snapshot_hash()));
        let bytes = serialize(&bank, Some(3));
        (bank, Some(bytes))
    } else {
        (full_bank, None)
    };
    let status_dir = tempfile::tempdir().unwrap();
    let status_path = status_dir.path().join("status_cache");
    serde_snapshot::serialize_status_cache(
        &bank.status_cache.read().unwrap().root_slot_deltas(),
        &status_path,
        &IoSetupState::default(),
    )
    .unwrap();
    let status = std::fs::read(status_path).unwrap();
    let image = image_stats(bank.slot(), bank.hash(), &accounts);
    assert_eq!(
        image.accounts_lt_hash,
        *bank.accounts_lt_hash.lock().unwrap()
    );
    assert_eq!(image.capitalization, bank.capitalization());
    assert_eq!(image.accounts_data_len, bank.load_accounts_data_size());
    Fixture {
        genesis,
        mint: info.mint_keypair,
        bank,
        full,
        incremental,
        status,
        manifest,
        accounts,
        image,
    }
}

impl Fixture {
    fn external(
        &self,
        image: &VerifiedSnapshotImage,
        status: &[u8],
        manifest: SnapshotManifest,
        view: Arc<SnapshotView>,
    ) -> std::result::Result<Bank, ExternalSnapshotError> {
        let mut full = BufReader::new(Cursor::new(&self.full));
        let mut incremental = self
            .incremental
            .as_ref()
            .map(|b| BufReader::new(Cursor::new(b)));
        bank_from_snapshot_streams_with_external_backend(
            &mut SnapshotStreams {
                full_snapshot_stream: &mut full,
                incremental_snapshot_stream: incremental.as_mut(),
            },
            status,
            manifest,
            image,
            &self.genesis,
            &RuntimeConfig::default(),
            view,
            PartitionedEpochRewardsConfig::default(),
        )
    }
    fn control(&self) -> Bank {
        let mut full = BufReader::new(Cursor::new(&self.full));
        let mut incremental = self
            .incremental
            .as_ref()
            .map(|b| BufReader::new(Cursor::new(b)));
        let (bank_fields, db_fields) = serde_snapshot::fields_from_streams(&mut SnapshotStreams {
            full_snapshot_stream: &mut full,
            incremental_snapshot_stream: incremental.as_mut(),
        })
        .unwrap();
        let storages = self.bank.get_snapshot_storages(None);
        let next = storages.iter().map(|s| s.id()).max().unwrap() + 1;
        let storage = AccountStorageMap::default();
        let storage_dir = tempfile::tempdir().unwrap();
        for entry in storages {
            // Real serialized account-file fixture, with fresh native storage
            // metadata. Sharing the source's Arc would incorrectly reuse its
            // live-account counters during index reconstruction.
            let path = storage_dir.path().join(entry.id().to_string());
            std::fs::copy(entry.accounts.path(), &path).unwrap();
            let (accounts_file, _) =
                solana_accounts_db::accounts_file::AccountsFile::new_from_file(
                    path,
                    entry.accounts.len(),
                )
                .unwrap();
            let restored =
                solana_accounts_db::account_storage_entry::AccountStorageEntry::new_existing(
                    entry.slot(),
                    entry.id(),
                    accounts_file,
                    solana_accounts_db::ObsoleteAccounts::default(),
                );
            storage.insert(entry.slot(), Arc::new(restored));
        }
        let (bank, info) = serde_snapshot::reconstruct_bank_from_fields(
            bank_fields,
            db_fields,
            &self.genesis,
            &RuntimeConfig::default(),
            &[],
            StorageAndNextAccountsFileId {
                storage,
                next_append_vec_id: AtomicAccountsFileId::new(next),
            },
            None,
            None,
            None,
            true,
            ACCOUNTS_DB_CONFIG_FOR_TESTING,
            None,
            Arc::default(),
        )
        .unwrap();
        verify_snapshot_capitalization(&bank, info.calculated_capitalization, false).unwrap();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("status_cache");
        std::fs::write(&path, &self.status).unwrap();
        let deltas = serde_snapshot::deserialize_status_cache(&path).unwrap();
        restore_snapshot_status_cache(&bank, &deltas).unwrap();
        let (slot, hash) = self.manifest.incremental.unwrap_or(self.manifest.full);
        verify_bank_against_expected_slot_hash(&bank, slot, hash).unwrap();
        assert!(bank.verify_snapshot_bank(
            true,
            false,
            self.manifest.full.0,
            Some(&info.calculated_accounts_lt_hash)
        ));
        bank
    }
}

fn compare(external: &Bank, native: &Bank, keys: impl IntoIterator<Item = Pubkey>) {
    assert_eq!(external.hash(), native.hash());
    assert_eq!(external.capitalization(), native.capitalization());
    assert_eq!(
        external.load_accounts_data_size(),
        native.load_accounts_data_size()
    );
    assert_eq!(external.get_bank_hash_stats(), native.get_bank_hash_stats());
    assert_eq!(external.epoch_stakes_map(), native.epoch_stakes_map());
    assert_eq!(
        external.stakes_cache.stakes().clone(),
        native.stakes_cache.stakes().clone()
    );
    assert_eq!(
        *external.status_cache.read().unwrap(),
        *native.status_cache.read().unwrap()
    );
    for key in keys {
        assert_eq!(
            external.get_account_with_fixed_root(&key),
            native.get_account_with_fixed_root(&key),
            "{key}"
        );
        assert_eq!(
            external
                .rc
                .accounts
                .load_with_fixed_root(&external.ancestors, &key),
            native
                .rc
                .accounts
                .load_with_fixed_root(&native.ancestors, &key),
            "modification slot: {key}"
        );
    }
}

#[test_case::test_case(false, false; "full")]
#[test_case::test_case(true, false; "full_and_incremental")]
#[test_case::test_case(false, true; "full_then_epoch")]
#[test_case::test_case(true, true; "incremental_then_epoch")]
fn native_external_snapshot_same_input(incremental: bool, cross_epoch: bool) {
    let fixture = fixture(incremental);
    let control = fixture.control();
    let view = SnapshotView::new(fixture.image.slot, fixture.accounts.clone());
    let external = fixture
        .external(
            &fixture.image,
            &fixture.status,
            fixture.manifest,
            view.clone(),
        )
        .unwrap();
    assert!(view.sealed.load(Relaxed));
    assert!(view.initialized.load(Relaxed));
    assert_eq!(
        view.stores.load(Relaxed),
        0,
        "frozen restore must not store accounts"
    );
    assert!(external.is_frozen());
    assert_eq!(external.bank_id(), 0);
    assert!(external.parent().is_none());
    compare(&external, &control, fixture.accounts.keys().copied());
    let external_forks = BankForks::new_rw_arc(external);
    let native_forks = BankForks::new_rw_arc(control);
    let external = external_forks.read().unwrap().root_bank();
    let control = native_forks.read().unwrap().root_bank();
    let slot = if cross_epoch {
        control
            .epoch_schedule()
            .get_first_slot_in_epoch(control.epoch() + 1)
    } else {
        external.slot() + 1
    };
    let leader = Bank::slot_leader_from_epoch_stakes(
        slot,
        control.epoch_schedule(),
        control.epoch_stakes_map(),
    );
    let external = Bank::new_from_parent(external, leader, slot);
    let control = Bank::new_from_parent(control, leader, slot);
    let to = Pubkey::new_unique();
    for bank in [&external, &control] {
        bank.transfer(567, &fixture.mint, &to).unwrap();
        bank.freeze();
    }
    compare(
        &external,
        &control,
        fixture.accounts.keys().copied().chain([to]),
    );
}

#[test]
fn native_external_snapshot_rejects_native_field_mismatches() {
    let mut fixture = fixture(false);
    for mutate in [
        (|f: &mut BankFieldsToSerialize| f.hash = Hash::new_unique())
            as fn(&mut BankFieldsToSerialize),
        |f| f.genesis_creation_time += 1,
        |f| f.leader_id = Pubkey::new_unique(),
        |f| f.versioned_epoch_stakes.clear(),
        |f| f.ticks_per_slot += 1,
    ] {
        fixture.full = serialize_with(&fixture.bank, None, mutate);
        // Bind the supplied image to the decoded hash even for the forged Bank
        // hash case. This must fail native hash_internal_state verification,
        // not merely a manifest/image identity comparison.
        let (fields, _) =
            serde_snapshot::fields_from_stream(&mut BufReader::new(Cursor::new(&fixture.full)))
                .unwrap();
        let mut image = fixture.image.clone();
        image.bank_hash = fields.hash;
        let view = SnapshotView::new(image.slot, fixture.accounts.clone());
        let result = fixture.external(&image, &fixture.status, fixture.manifest, view.clone());
        assert!(result.is_err());
        assert!(!view.sealed.load(Relaxed));
    }
    fixture.full.truncate(12);
    let view = SnapshotView::new(fixture.image.slot, fixture.accounts.clone());
    assert!(matches!(
        fixture.external(&fixture.image, &fixture.status, fixture.manifest, view),
        Err(ExternalSnapshotError::Decode(_))
    ));
}

#[test]
fn native_external_snapshot_rejects_mismatches() {
    let fixture = fixture(false);
    let reject = |image: &VerifiedSnapshotImage, status: &[u8], manifest: SnapshotManifest| {
        let view = SnapshotView::new(fixture.image.slot, fixture.accounts.clone());
        let result = fixture.external(image, status, manifest, view.clone());
        assert!(result.is_err());
        assert!(!view.sealed.load(Relaxed));
        assert!(!view.rooted.load(Relaxed));
    };
    let mut image = fixture.image.clone();
    image.capitalization += 1;
    reject(&image, &fixture.status, fixture.manifest);
    let mut image = fixture.image.clone();
    image.accounts_lt_hash = AccountsLtHash(LtHash::identity());
    reject(&image, &fixture.status, fixture.manifest);
    let mut image = fixture.image.clone();
    image.bank_hash = Hash::new_unique();
    reject(&image, &fixture.status, fixture.manifest);
    let mut image = fixture.image.clone();
    image.accounts_data_len += 1;
    reject(&image, &fixture.status, fixture.manifest);
    let mut manifest = fixture.manifest;
    manifest.full.0 += 1;
    reject(&fixture.image, &fixture.status, manifest);
    let mut manifest = fixture.manifest;
    manifest.full.1 = SnapshotHash::new(LtHash::identity().checksum());
    reject(&fixture.image, &fixture.status, manifest);
    reject(&fixture.image, &[1, 2, 3], fixture.manifest);
    let mut status = fixture.status.clone();
    status.push(0);
    reject(&fixture.image, &status, fixture.manifest);
    // Well-formed native encoding, but missing all required status roots.
    reject(&fixture.image, &0u64.to_le_bytes(), fixture.manifest);
}

#[test_case::test_case(false; "full")]
#[test_case::test_case(true; "incremental")]
fn native_external_snapshot_rejects_trailing_bytes_before_initialize(incremental: bool) {
    let mut fixture = fixture(true);
    if incremental {
        fixture.incremental.as_mut().unwrap().push(0);
    } else {
        fixture.full.push(0);
    }
    let view = SnapshotView::new(fixture.image.slot, fixture.accounts.clone());
    let result = fixture.external(
        &fixture.image,
        &fixture.status,
        fixture.manifest,
        view.clone(),
    );
    assert!(
        matches!(result, Err(ExternalSnapshotError::Snapshot(SnapshotError::Io(error)))
        if error.to_string().starts_with("invalid snapshot data file"))
    );
    assert!(!view.initialized.load(Relaxed));
    assert!(!view.sealed.load(Relaxed));
    assert_eq!(view.stores.load(Relaxed), 0);
}

/// Models a seekable oversized file without allocating or reading its contents.
struct LengthOnlyReader {
    length: u64,
    position: u64,
}

impl Read for LengthOnlyReader {
    fn read(&mut self, _: &mut [u8]) -> std::io::Result<usize> {
        panic!("the total file size must be rejected before decoding either stream");
    }
}

impl Seek for LengthOnlyReader {
    fn seek(&mut self, position: SeekFrom) -> std::io::Result<u64> {
        self.position = match position {
            SeekFrom::Start(position) => position,
            SeekFrom::End(offset) => self.length.checked_add_signed(offset).unwrap(),
            SeekFrom::Current(offset) => self.position.checked_add_signed(offset).unwrap(),
        };
        Ok(self.position)
    }
}

#[test_case::test_case(false; "full")]
#[test_case::test_case(true; "incremental")]
fn native_external_snapshot_rejects_total_size_before_initialize(incremental: bool) {
    let fixture = fixture(true);
    let maximum = crate::snapshot_utils::MAX_SNAPSHOT_DATA_FILE_SIZE;
    let mut full = BufReader::new(LengthOnlyReader {
        length: if incremental { maximum } else { maximum + 1 },
        position: 0,
    });
    let mut delta = BufReader::new(LengthOnlyReader {
        length: maximum + 1,
        position: 0,
    });
    let view = SnapshotView::new(fixture.image.slot, fixture.accounts.clone());
    let result = bank_from_snapshot_streams_with_external_backend(
        &mut SnapshotStreams {
            full_snapshot_stream: &mut full,
            incremental_snapshot_stream: Some(&mut delta),
        },
        &fixture.status,
        fixture.manifest,
        &fixture.image,
        &fixture.genesis,
        &RuntimeConfig::default(),
        view.clone(),
        PartitionedEpochRewardsConfig::default(),
    );
    assert!(
        matches!(result, Err(ExternalSnapshotError::Snapshot(SnapshotError::Io(error)))
        if error.to_string().starts_with("too large snapshot data file to deserialize"))
    );
    assert!(!view.initialized.load(Relaxed));
    assert!(!view.sealed.load(Relaxed));
    assert_eq!(view.stores.load(Relaxed), 0);
}
