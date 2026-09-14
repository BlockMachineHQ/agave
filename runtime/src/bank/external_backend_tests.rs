use {
    super::*,
    crate::genesis_utils::{
        bootstrap_validator_stake_lamports, create_genesis_config_with_leader_ex,
    },
    solana_accounts_db::{
        external_backend::{BackendResult, BankIdentity, ExternalAccountBackend},
        partitioned_rewards::PartitionedEpochRewardsConfig,
    },
    solana_signer::Signer,
    std::collections::HashMap,
};

#[derive(Default)]
struct Events {
    fault: Option<String>,
    applied: Vec<Vec<BankIdentity>>,
    sealed: HashSet<BankIdentity>,
    parents: HashMap<BankIdentity, Option<BankIdentity>>,
    barrier: Option<(std::sync::mpsc::Sender<()>, std::sync::mpsc::Receiver<()>)>,
    observed_roots: Option<crate::bank_forks::SharableBanks>,
    approved_roots: HashSet<BankIdentity>,
    rooted: Vec<BankIdentity>,
    released: Vec<BankIdentity>,
    removed: HashSet<BankIdentity>,
}

impl std::fmt::Debug for Events {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Events")
            .field("applied", &self.applied)
            .finish_non_exhaustive()
    }
}

#[derive(Debug, Default)]
struct ViewState {
    initialized: bool,
    sealed: bool,
    accounts: HashMap<Pubkey, (AccountSharedData, Slot)>,
}

/// Deliberately simple reference view: copy on fork, retained immutable readers.
/// There is no AccountsDb in this fixture's external path.
#[derive(Debug)]
struct MemoryView {
    id: BankIdentity,
    parent: Option<BankIdentity>,
    state: Mutex<ViewState>,
    events: Arc<Mutex<Events>>,
}

impl MemoryView {
    fn genesis() -> Arc<Self> {
        Arc::new(Self {
            id: BankIdentity {
                slot: 0,
                bank_id: 0,
            },
            parent: None,
            state: Mutex::default(),
            events: Arc::default(),
        })
    }
}

impl ExternalAccountBackend for MemoryView {
    fn identity(&self) -> BankIdentity {
        self.id
    }
    fn parent_identity(&self) -> Option<BankIdentity> {
        self.parent
    }
    fn initialize_empty_genesis(&self) -> BackendResult<()> {
        let mut state = self.state.lock().unwrap();
        if state.initialized || !state.accounts.is_empty() || self.parent.is_some() {
            return Err("genesis view is not empty and fresh".into());
        }
        state.initialized = true;
        self.events.lock().unwrap().parents.insert(self.id, None);
        Ok(())
    }
    fn load(&self, key: &Pubkey) -> BackendResult<Option<(AccountSharedData, Slot)>> {
        if self.events.lock().unwrap().fault.as_deref() == Some("load") {
            return Err("injected load failure".into());
        }
        if self.events.lock().unwrap().removed.contains(&self.id) {
            return Err("bank was invalidated".into());
        }
        Ok(self
            .state
            .lock()
            .unwrap()
            .accounts
            .get(key)
            .cloned()
            .filter(|(account, _)| account.lamports() != 0))
    }
    fn store(&self, accounts: &[(Pubkey, AccountSharedData)]) -> BackendResult<()> {
        if self.events.lock().unwrap().fault.as_deref() == Some("store") {
            return Err("injected store failure".into());
        }
        let mut state = self.state.lock().unwrap();
        if state.sealed {
            return Err("store after seal".into());
        }
        for (key, account) in accounts {
            state.accounts.insert(*key, (account.clone(), self.id.slot));
        }
        Ok(())
    }
    fn fork_child(&self, child: BankIdentity) -> BackendResult<Arc<dyn ExternalAccountBackend>> {
        let state = self.state.lock().unwrap();
        if !state.sealed {
            return Err("fork before seal".into());
        }
        self.events
            .lock()
            .unwrap()
            .parents
            .insert(child, Some(self.id));
        Ok(Arc::new(Self {
            id: child,
            parent: Some(self.id),
            state: Mutex::new(ViewState {
                initialized: true,
                sealed: false,
                accounts: state.accounts.clone(),
            }),
            events: self.events.clone(),
        }))
    }
    fn seal(&self) -> BackendResult<()> {
        if self.events.lock().unwrap().fault.as_deref() == Some("seal") {
            return Err("injected seal failure".into());
        }
        self.state.lock().unwrap().sealed = true;
        self.events.lock().unwrap().sealed.insert(self.id);
        Ok(())
    }
    fn apply_root(&self, path: &[BankIdentity]) -> BackendResult<()> {
        let barrier = self.events.lock().unwrap().barrier.take();
        if let Some((entered, release)) = barrier {
            entered.send(()).unwrap();
            release
                .recv_timeout(std::time::Duration::from_secs(10))
                .unwrap();
        }
        let mut events = self.events.lock().unwrap();
        assert_eq!(path.last(), Some(&self.id));
        for (index, id) in path.iter().enumerate() {
            assert!(events.sealed.contains(id));
            if index > 0 {
                assert_eq!(events.parents[id], Some(path[index - 1]));
            }
        }
        if events.fault.as_deref() == Some("apply_root") {
            let observed = events.observed_roots.as_ref().unwrap();
            assert_eq!(observed.root().slot(), 0);
            assert_eq!(
                events.rooted.iter().map(|id| id.slot).collect::<Vec<_>>(),
                vec![0]
            );
            eprintln!("ROOT_BARRIER_FAILED_OLD_ROOT=0_NO_PUBLICATION");
            return Err("injected root application failure".into());
        }
        events.approved_roots.extend(path.iter().copied());
        events.applied.push(path.to_vec());
        Ok(())
    }
    fn mark_root(&self) -> BackendResult<()> {
        if !self.state.lock().unwrap().sealed {
            return Err("root before seal".into());
        }
        let mut events = self.events.lock().unwrap();
        if !events.approved_roots.contains(&self.id) {
            return Err("missing root barrier receipt".into());
        }
        events.rooted.push(self.id);
        Ok(())
    }
    fn remove_unrooted(&self, banks: &[BankIdentity]) -> BackendResult<()> {
        let mut events = self.events.lock().unwrap();
        for bank in banks {
            if events.rooted.contains(bank) {
                return Err("cannot remove root".into());
            }
            events.removed.insert(*bank);
        }
        Ok(())
    }
    fn release_bank(&self) -> BackendResult<()> {
        self.events.lock().unwrap().released.push(self.id);
        Ok(())
    }
}

fn pair() -> (Bank, Bank, Arc<MemoryView>, solana_keypair::Keypair) {
    let mint = solana_keypair::keypair_from_seed(&[1; 32]).unwrap();
    let vote = solana_keypair::keypair_from_seed(&[2; 32]).unwrap();
    let bls = solana_bls_signatures::keypair::Keypair::derive_from_signer(
        &vote,
        agave_votor_messages::consensus_message::BLS_KEYPAIR_DERIVE_SEED,
    )
    .unwrap();
    let mut genesis = create_genesis_config_with_leader_ex(
        10_000_000_000,
        &mint.pubkey(),
        &Pubkey::from([3; 32]),
        &vote.pubkey(),
        &Pubkey::from([4; 32]),
        Some(bls.public.to_bytes_compressed()),
        bootstrap_validator_stake_lamports(),
        890_880,
        FeeRateGovernor::new(0, 0),
        Rent::free(),
        ClusterType::Development,
        &FeatureSet::all_enabled(),
        vec![],
    );
    genesis.creation_time = 1_700_000_000;
    let view = MemoryView::genesis();
    let external = Bank::new_from_genesis_with_external_backend(
        &genesis,
        Arc::default(),
        view.clone(),
        PartitionedEpochRewardsConfig::default(),
    );
    let native = Bank::new_from_genesis(
        &genesis,
        Arc::default(),
        vec![],
        None,
        ACCOUNTS_DB_CONFIG_FOR_TESTING,
        None,
        None,
        Arc::default(),
        None,
        None,
    );
    (external, native, view, mint)
}

fn assert_equal(external: &Bank, native: &Bank, keys: &[Pubkey]) {
    for key in keys {
        assert_eq!(external.get_account(key), native.get_account(key));
        assert_eq!(
            external.get_account_with_fixed_root(key),
            native.get_account_with_fixed_root(key)
        );
    }
    external.freeze();
    native.freeze();
    assert_eq!(external.hash(), native.hash());
    assert_eq!(external.capitalization(), native.capitalization());
    assert_eq!(
        external.load_accounts_data_size(),
        native.load_accounts_data_size()
    );
}

#[test]
fn native_external_parent_sibling_freeze() {
    let (external, native, view, mint) = pair();
    let key = Pubkey::from([5; 32]);
    let original = AccountSharedData::new(2_000_000, 0, &solana_system_interface::program::id());
    for bank in [&external, &native] {
        bank.store_account(&key, &original);
    }
    let table_key = Pubkey::from([6; 32]);
    let table = solana_address_lookup_table_interface::state::AddressLookupTable {
        meta: solana_address_lookup_table_interface::state::LookupTableMeta::default(),
        addresses: std::borrow::Cow::Owned(vec![key]),
    };
    let table_account = AccountSharedData::create_from_existing_shared_data(
        2_000_000,
        Arc::new(table.serialize_for_tests().unwrap()),
        solana_address_lookup_table_interface::program::id(),
        false,
        0,
    );
    for bank in [&external, &native] {
        bank.store_account(&table_key, &table_account);
    }
    assert_equal(&external, &native, &[key, mint.pubkey()]);
    let external_forks = BankForks::new_rw_arc(external);
    let native_forks = BankForks::new_rw_arc(native);
    let external = external_forks.read().unwrap().get(0).unwrap();
    let native = native_forks.read().unwrap().get(0).unwrap();
    let leader = *external.leader();
    let child = Arc::new(Bank::new_from_parent(external.clone(), leader, 1));
    let control = Arc::new(Bank::new_from_parent(native.clone(), leader, 1));
    let sibling = Arc::new(Bank::new_from_parent(external.clone(), leader, 1));
    assert_ne!(child.bank_id(), sibling.bank_id());
    let lookup = solana_message::v0::MessageAddressTableLookup {
        account_key: table_key,
        writable_indexes: vec![0],
        readonly_indexes: vec![],
    };
    for bank in [&child, &control] {
        let (addresses, deactivation_slot) = bank
            .rc
            .accounts
            .load_lookup_table_addresses(
                &bank.ancestors,
                (&lookup).into(),
                &solana_slot_hashes::SlotHashes::default(),
            )
            .unwrap();
        assert_eq!(addresses.writable, vec![key]);
        assert_eq!(deactivation_slot, u64::MAX);
        assert_eq!(
            bank.rc
                .accounts
                .load_with_fixed_root(&bank.ancestors, &table_key)
                .unwrap()
                .1,
            0
        );
    }
    for bank in [&child, &control] {
        let tx = solana_system_transaction::transfer(&mint, &key, 1_000_000, bank.last_blockhash());
        bank.process_transaction(&tx).unwrap();
        // A native nontransaction write after the transaction must hash its old value.
        bank.store_account(
            &key,
            &AccountSharedData::new(4_000_000, 0, &solana_system_interface::program::id()),
        );
    }
    let repeated = [
        (
            key,
            AccountSharedData::new(5_000_000, 0, &solana_system_interface::program::id()),
        ),
        (key, AccountSharedData::default()),
        (
            key,
            AccountSharedData::new(6_000_000, 0, &solana_system_interface::program::id()),
        ),
    ];
    for bank in [&child, &control] {
        // Exercise the shared ordered transaction-store wrapper and native
        // last-write hash selection with repeated keys in one batch.
        let batch = (bank.slot(), &repeated[..]);
        bank.enqueue_on_chain_accounts_lt_hash_updates(&batch);
        bank.rc
            .accounts
            .store_accounts_seq(batch, None, &bank.ancestors);
    }
    assert_eq!(child.get_account(&key).unwrap().lamports(), 6_000_000);
    assert_eq!(sibling.get_account(&key), Some(original.clone()));
    assert_eq!(external.get_account(&key), Some(original.clone()));
    assert_eq!(
        child
            .rc
            .accounts
            .load_with_fixed_root(&child.ancestors, &key)
            .unwrap()
            .1,
        1
    );
    assert_equal(&child, &control, &[key, mint.pubkey()]);
    let grandchild = Bank::new_from_parent(child.clone(), leader, 2);
    let grandcontrol = Bank::new_from_parent(control.clone(), leader, 2);
    for bank in [&grandchild, &grandcontrol] {
        bank.store_account(&key, &AccountSharedData::default());
    }
    assert_equal(&grandchild, &grandcontrol, &[key, mint.pubkey()]);
    assert_eq!(grandchild.get_account(&key), None);
    let frozen = grandchild.frozen_external_account_pin().unwrap();
    assert_eq!(
        grandchild
            .rc
            .accounts
            .external_backend()
            .unwrap()
            .parent_identity()
            .unwrap()
            .bank_id,
        child.bank_id()
    );
    let grandchild = external_forks.write().unwrap().insert(grandchild);
    external_forks.write().unwrap().set_root(2, None, None);
    assert_eq!(
        view.events
            .lock()
            .unwrap()
            .rooted
            .iter()
            .map(|id| id.slot)
            .collect::<Vec<_>>(),
        vec![0, 0, 1, 2]
    );
    assert_eq!(external.get_account(&key), Some(original.clone()));
    assert_eq!(sibling.get_account(&key), Some(original));
    let sibling_id = BankIdentity {
        slot: sibling.slot(),
        bank_id: sibling.bank_id(),
    };
    grandchild.remove_unrooted_slots(&[(sibling_id.slot, sibling_id.bank_id)]);
    drop(sibling);
    assert!(view.events.lock().unwrap().released.contains(&sibling_id));
    drop(grandchild);
    assert_eq!(frozen.load(&key).unwrap(), None);
}

#[test]
fn native_external_read_only_frozen_pin() {
    let (bank, native, view, mint) = pair();
    assert!(native.frozen_external_account_pin().is_none());
    assert!(!bank.is_frozen());
    assert!(
        std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            bank.frozen_external_account_pin()
        }))
        .is_err()
    );
    // Invalid pin acquisition is a native API precondition panic, not a provider
    // failure: no freeze, receipt, or root mutation occurred.
    assert!(!bank.is_frozen());
    assert!(view.events.lock().unwrap().sealed.is_empty());
    let expected = bank.get_account(&mint.pubkey()).unwrap();
    bank.freeze();
    let pin = bank.frozen_external_account_pin().unwrap();
    assert_eq!(pin.identity(), view.id);
    assert_eq!(
        pin.load(&mint.pubkey()).unwrap(),
        Some((expected.clone(), 0))
    );
    assert_eq!(pin.load(&Pubkey::new_unique()).unwrap(), None);
    view.events.lock().unwrap().fault = Some("load".into());
    assert!(pin.load(&mint.pubkey()).is_err());
    view.events.lock().unwrap().fault = None;
    let retained = pin.clone();
    drop(pin);
    drop(bank);
    assert!(
        view.events
            .lock()
            .unwrap()
            .released
            .contains(&retained.identity())
    );
    assert_eq!(retained.load(&mint.pubkey()).unwrap(), Some((expected, 0)));
    let events = view.events.lock().unwrap();
    assert!(events.applied.is_empty());
    assert!(events.rooted.is_empty());
}

#[test]
fn native_external_root_barrier_malformed_refuses_before_mutation() {
    let (bank, native, view, _mint) = pair();
    let forks = BankForks::new_rw_arc(bank);
    let root = forks.read().unwrap().root_bank();
    let leader = *root.leader();
    let active = Arc::new(Bank::new_from_parent(root.clone(), leader, 1));
    let refuse = |target: &Arc<Bank>, previous: &Arc<Bank>, snapshot, message: &str| {
        let parent = target.parent().map(|bank| bank.bank_id());
        let frozen = target.is_frozen();
        let status_roots = target.status_cache.read().unwrap().roots().clone();
        let error = BankForks::apply_external_root(target, Some(previous), snapshot).unwrap_err();
        assert!(error.to_string().contains(message), "{error}");
        assert_eq!(target.parent().map(|bank| bank.bank_id()), parent);
        assert_eq!(target.is_frozen(), frozen);
        assert_eq!(*target.status_cache.read().unwrap().roots(), status_roots);
        let forks = forks.read().unwrap();
        assert_eq!(forks.root(), 0);
        assert!(Arc::ptr_eq(&forks.root_bank(), &root));
        let events = view.events.lock().unwrap();
        assert_eq!(events.applied, vec![vec![view.id]]);
        assert_eq!(events.rooted, vec![view.id]);
    };
    refuse(&active, &root, false, "frozen");
    // Snapshot rejection precedes even the frozen-path check.
    refuse(&active, &root, true, "snapshot controller");
    active.freeze();
    let sibling = Arc::new(Bank::new_from_parent(root.clone(), leader, 1));
    sibling.freeze();
    refuse(&sibling, &active, false, "exact descendant");
    refuse(&root, &active, false, "exact descendant");
    let mut malformed = Bank::new_from_parent(root.clone(), leader, 2);
    malformed.freeze();
    let id = BankIdentity {
        slot: malformed.slot(),
        bank_id: malformed.bank_id(),
    };
    let replace_backend = |bank: &mut Bank, backend_id, parent| {
        bank.rc.accounts = Arc::new(Accounts::new_external(
            Arc::new(MemoryView {
                id: backend_id,
                parent,
                state: Mutex::new(ViewState {
                    initialized: true,
                    sealed: true,
                    accounts: HashMap::new(),
                }),
                events: view.events.clone(),
            }),
            1,
        ));
    };
    replace_backend(
        &mut malformed,
        id,
        Some(BankIdentity {
            slot: 0,
            bank_id: u64::MAX,
        }),
    );
    let mut malformed = Arc::new(malformed);
    refuse(&malformed, &root, false, "parent identity mismatch");
    replace_backend(
        Arc::get_mut(&mut malformed).unwrap(),
        BankIdentity {
            slot: 2,
            bank_id: u64::MAX,
        },
        Some(view.id),
    );
    refuse(&malformed, &root, false, "Bank identity mismatch");
    Arc::get_mut(&mut malformed).unwrap().rc.accounts = native.rc.accounts.clone();
    refuse(&malformed, &root, false, "mixed native/external");
    // An unfrozen intermediate Bank is refused even when the target is frozen.
    let grandchild = Arc::new(Bank::new_from_parent(active.clone(), leader, 3));
    grandchild.freeze();
    let hash = active.hash();
    *active.hash.write().unwrap() = Hash::default();
    refuse(&grandchild, &root, false, "frozen");
    *active.hash.write().unwrap() = hash;
}

#[test]
fn native_external_root_barrier_publication() {
    let (bank, _native, view, _mint) = pair();
    let forks = BankForks::new_rw_arc(bank);
    let root = forks.read().unwrap().root_bank();
    assert_eq!(view.events.lock().unwrap().applied, vec![vec![view.id]]);
    let child = Bank::new_from_parent(root.clone(), *root.leader(), 1);
    child.freeze();
    let child = forks
        .write()
        .unwrap()
        .insert(child)
        .clone_without_scheduler();
    let target = Bank::new_from_parent(child.clone(), *child.leader(), 3);
    target.freeze();
    let target = forks
        .write()
        .unwrap()
        .insert(target)
        .clone_without_scheduler();
    let expected = [&root, &child, &target].map(|bank| BankIdentity {
        slot: bank.slot(),
        bank_id: bank.bank_id(),
    });
    let sharable = forks.read().unwrap().sharable_banks();
    let (entered_tx, entered_rx) = std::sync::mpsc::channel();
    let (release_tx, release_rx) = std::sync::mpsc::channel();
    view.events.lock().unwrap().barrier = Some((entered_tx, release_rx));
    let writer = forks.clone();
    let thread = std::thread::spawn(move || writer.write().unwrap().set_root(3, None, None));
    entered_rx
        .recv_timeout(std::time::Duration::from_secs(10))
        .unwrap();
    assert!(Arc::ptr_eq(&sharable.root(), &root));
    assert_eq!(view.events.lock().unwrap().rooted, vec![view.id]);
    assert!(target.parent().is_some());
    release_tx.send(()).unwrap();
    thread.join().unwrap();
    assert_eq!(forks.read().unwrap().root(), 3);
    assert!(Arc::ptr_eq(&sharable.root(), &target));
    assert_eq!(view.events.lock().unwrap().applied[1], expected);
    assert_eq!(&view.events.lock().unwrap().rooted[1..], &expected);
    assert!(target.parent().is_none());
    // Repeating a root and advancing after squash both retain valid receipts.
    forks.write().unwrap().set_root(3, None, None);
    let next = Bank::new_from_parent(target.clone(), *target.leader(), 4);
    next.freeze();
    let next_id = next.frozen_external_account_pin().unwrap().identity();
    forks.write().unwrap().insert(next);
    forks.write().unwrap().set_root(4, None, None);
    assert_eq!(
        view.events.lock().unwrap().applied.last().unwrap(),
        &vec![expected[2], next_id]
    );
}

#[test]
fn native_external_root_barrier_failure() {
    const ENV: &str = "AGAVE_ROOT_BARRIER_FAILURE";
    if std::env::var_os(ENV).is_some() {
        let (bank, _native, view, _mint) = pair();
        let forks = BankForks::new_rw_arc(bank);
        let root = forks.read().unwrap().root_bank();
        let child = Bank::new_from_parent(root.clone(), *root.leader(), 1);
        child.freeze();
        forks.write().unwrap().insert(child);
        {
            let mut events = view.events.lock().unwrap();
            events.fault = Some("apply_root".into());
            events.observed_roots = Some(forks.read().unwrap().sharable_banks());
        }
        forks.write().unwrap().set_root(1, None, None);
        eprintln!("ROOT_WAS_PUBLISHED");
        panic!("root failure returned");
    }
    let output = std::process::Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "bank::external_backend_tests::native_external_root_barrier_failure",
            "--nocapture",
        ])
        .env(ENV, "1")
        .output()
        .unwrap();
    use std::os::unix::process::ExitStatusExt;
    assert_eq!(output.status.signal(), Some(6));
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("ROOT_BARRIER_FAILED_OLD_ROOT=0_NO_PUBLICATION"),
        "{stderr}"
    );
    assert!(!stderr.contains("ROOT_WAS_PUBLISHED"));
}

#[test]
fn native_external_unsupported_operations_refuse() {
    let (bank, _native, _view, _mint) = pair();
    for operation in [Bank::force_flush_accounts_cache, |bank: &Bank| {
        let _ = bank.calculate_accounts_data_size();
    }] {
        let failure = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| operation(&bank)));
        let panic = failure.expect_err("native-only operation must refuse external backend");
        let message = panic
            .downcast_ref::<String>()
            .map(String::as_str)
            .or_else(|| panic.downcast_ref::<&str>().copied())
            .unwrap();
        assert!(message.contains("unsupported native AccountsDb operation"));
    }
}

#[test]
fn native_external_terminal_faults() {
    const FAULT_ENV: &str = "AGAVE_EXTERNAL_BACKEND_TEST_FAULT";
    if let Ok(fault) = std::env::var(FAULT_ENV) {
        let (bank, _native, view, _mint) = pair();
        let key = Pubkey::new_unique();
        let account = AccountSharedData::new(1, 0, &Pubkey::default());
        view.events.lock().unwrap().fault = Some(if fault == "worker" {
            "load".into()
        } else {
            fault.clone()
        });
        match fault.as_str() {
            "load" => {
                let _ = bank.get_account(&key);
            }
            "store" => bank.store_account(&key, &account),
            "seal" => bank.freeze(),
            "root" => {
                bank.squash();
            }
            "worker" => {
                let pool = rayon::ThreadPoolBuilder::new()
                    .num_threads(2)
                    .build()
                    .unwrap();
                bank.store_accounts((0, &[(key, account)][..]), Some(&pool));
            }
            _ => panic!("unknown fault"),
        }
        panic!("terminal backend fault returned to caller");
    }
    for fault in ["load", "store", "seal", "root", "worker"] {
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "bank::external_backend_tests::native_external_terminal_faults",
                "--nocapture",
            ])
            .env(FAULT_ENV, fault)
            .output()
            .unwrap();
        use std::os::unix::process::ExitStatusExt;
        assert_eq!(
            output.status.signal(),
            Some(6),
            "fault {fault}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(
            String::from_utf8_lossy(&output.stderr)
                .contains("terminal external account backend failure")
        );
    }
}
