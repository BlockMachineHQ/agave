#[cfg(feature = "metrics")]
use solana_program_runtime::program_metrics::LoadProgramMetrics;
use {
    solana_account::{AccountSharedData, ReadableAccount, state_traits::StateMut},
    solana_clock::Slot,
    solana_instruction::error::InstructionError,
    solana_loader_v3_interface::state::UpgradeableLoaderState,
    solana_loader_v4_interface::state::{LoaderV4State, LoaderV4Status},
    solana_program_runtime::{
        loaded_programs::{
            ProgramCacheForTxBatch, ProgramCacheMatchCriteria, ProgramRuntimeEnvironment,
            ProgramToLoad,
        },
        program_cache_entry::{
            DELAY_VISIBILITY_SLOT_OFFSET, ProgramCacheEntry, ProgramCacheEntryOwner,
            ProgramCacheEntryType,
        },
    },
    solana_pubkey::Pubkey,
    solana_sdk_ids::{bpf_loader, bpf_loader_deprecated, bpf_loader_upgradeable, loader_v4},
    solana_svm_callback::TransactionProcessingCallback,
    solana_svm_timings::ExecuteTimings,
    solana_svm_type_overrides::sync::{Arc, Weak},
    solana_transaction_error::{TransactionError, TransactionResult},
    std::sync::atomic::Ordering,
};

/// Exact resolved program inputs, independent of the execution slot and environment.
/// SHA-256 binds full account bytes, addresses, owners, executable flags and
/// modification slots, plus native account size and deployment/effective slots.
/// Balances are excluded; account resolution still performs its ordinary liveness checks.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct ProgramAccountIdentity([u8; 32]);

impl ProgramAccountIdentity {
    pub fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }
}

/// Native verification provenance. Does not retain code, source accounts or an environment.
#[derive(Clone)]
pub struct VerifiedProgramReceipt {
    identity: ProgramAccountIdentity,
    entry: Weak<ProgramCacheEntry>,
}

impl VerifiedProgramReceipt {
    pub fn identity(&self) -> &ProgramAccountIdentity {
        &self.identity
    }

    /// Temporarily upgrades the native entry for pointer-safe retention/accounting.
    /// The receipt alone cannot keep the payload alive or certify another entry.
    pub fn entry(&self) -> Option<Arc<ProgramCacheEntry>> {
        self.entry.upgrade()
    }
}

/// Optional per-call reuse of previously verified native loads. Implementations own
/// admission and retention; an invalid or expired receipt is always a cold miss.
pub trait ProgramLoadCache: Sync {
    fn lookup(
        &self,
        identity: &ProgramAccountIdentity,
        environment: &ProgramRuntimeEnvironment,
    ) -> Option<VerifiedProgramReceipt>;

    /// Called only after a successful verified `ProgramCacheEntry::new`, not on
    /// reuse, failed verification, deployment, or upcoming-environment preparation.
    fn on_load(&self, receipt: &VerifiedProgramReceipt);
}

#[derive(Debug)]
pub(crate) enum ProgramAccountLoadResult {
    InvalidAccountData(ProgramCacheEntryOwner),
    ProgramOfLoaderV1(AccountSharedData),
    ProgramOfLoaderV2(AccountSharedData),
    ProgramOfLoaderV3(AccountSharedData, AccountSharedData, Slot, Pubkey, Slot),
    ProgramOfLoaderV4(AccountSharedData, Slot),
}

pub(crate) fn load_program_accounts<CB: TransactionProcessingCallback>(
    callbacks: &CB,
    pubkey: &Pubkey,
) -> Option<(ProgramAccountLoadResult, Slot)> {
    let (program_account, last_modification_slot) = callbacks.get_account_shared_data(pubkey)?;

    let load_result = if loader_v4::check_id(program_account.owner()) {
        loader_v4_get_state(program_account.data())
            .ok()
            .and_then(|state| {
                (!matches!(state.status, LoaderV4Status::Retracted)).then_some(state.slot)
            })
            .map(|slot| ProgramAccountLoadResult::ProgramOfLoaderV4(program_account, slot))
            .unwrap_or(ProgramAccountLoadResult::InvalidAccountData(
                ProgramCacheEntryOwner::LoaderV4,
            ))
    } else if bpf_loader_upgradeable::check_id(program_account.owner()) {
        if let Ok(UpgradeableLoaderState::Program {
            programdata_address,
        }) = program_account.state()
        {
            if let Some((programdata_account, programdata_modification_slot)) =
                callbacks.get_account_shared_data(&programdata_address)
            {
                if bpf_loader_upgradeable::check_id(programdata_account.owner()) {
                    if let Ok(UpgradeableLoaderState::ProgramData {
                        slot,
                        upgrade_authority_address: _,
                    }) = programdata_account.state()
                    {
                        ProgramAccountLoadResult::ProgramOfLoaderV3(
                            program_account,
                            programdata_account,
                            slot,
                            programdata_address,
                            programdata_modification_slot,
                        )
                    } else {
                        ProgramAccountLoadResult::InvalidAccountData(
                            ProgramCacheEntryOwner::LoaderV3,
                        )
                    }
                } else {
                    ProgramAccountLoadResult::InvalidAccountData(ProgramCacheEntryOwner::LoaderV3)
                }
            } else {
                ProgramAccountLoadResult::InvalidAccountData(ProgramCacheEntryOwner::LoaderV3)
            }
        } else {
            ProgramAccountLoadResult::InvalidAccountData(ProgramCacheEntryOwner::LoaderV3)
        }
    } else if bpf_loader::check_id(program_account.owner()) {
        ProgramAccountLoadResult::ProgramOfLoaderV2(program_account)
    } else if bpf_loader_deprecated::check_id(program_account.owner()) {
        ProgramAccountLoadResult::ProgramOfLoaderV1(program_account)
    } else {
        return None;
    };

    Some((load_result, last_modification_slot))
}

/// Loads the program with the given pubkey.
///
/// If the account doesn't exist it returns `None`. If the account does exist, it must be a program
/// account (belong to one of the program loaders). Returns `Some(InvalidAccountData)` if the program
/// account is `Closed`, contains invalid data or any of the programdata accounts are invalid.
pub fn load_program_with_pubkey<CB: TransactionProcessingCallback>(
    callbacks: &CB,
    program_runtime_environment: &ProgramRuntimeEnvironment,
    pubkey: &Pubkey,
    current_slot: Slot,
    execute_timings: &mut ExecuteTimings,
) -> Option<(Arc<ProgramCacheEntry>, Slot)> {
    load_program_with_pubkey_and_cache(
        callbacks,
        program_runtime_environment,
        pubkey,
        current_slot,
        execute_timings,
        None,
    )
}

/// Loads through the ordinary native resolver, optionally recovering verified code.
pub fn load_program_with_pubkey_and_cache<CB: TransactionProcessingCallback>(
    callbacks: &CB,
    program_runtime_environment: &ProgramRuntimeEnvironment,
    pubkey: &Pubkey,
    current_slot: Slot,
    execute_timings: &mut ExecuteTimings,
    program_load_cache: Option<&dyn ProgramLoadCache>,
) -> Option<(Arc<ProgramCacheEntry>, Slot)> {
    #[cfg(feature = "metrics")]
    let mut load_program_metrics = LoadProgramMetrics {
        program_id: pubkey.to_string(),
        ..LoadProgramMetrics::default()
    };
    #[cfg(not(feature = "metrics"))]
    let _ = execute_timings;

    let (load_result, last_modification_slot) = load_program_accounts(callbacks, pubkey)?;
    #[cfg_attr(not(feature = "metrics"), allow(unused_mut))]
    let mut load = |program_account: &AccountSharedData,
                    programdata: Option<(&Pubkey, &AccountSharedData, Slot)>,
                    deployment_slot: Slot,
                    elf_bytes: &[u8],
                    account_size: usize| {
        let effective_slot = deployment_slot.saturating_add(DELAY_VISIBILITY_SLOT_OFFSET);
        let identity = program_load_cache.map(|_| {
            let mut hasher = solana_sha256_hasher::Hasher::default();
            let mut field = |bytes: &[u8]| {
                hasher.hash(&(bytes.len() as u64).to_le_bytes());
                hasher.hash(bytes);
            };
            field(b"solana-svm:verified-program-inputs:v1");
            field(pubkey.as_ref());
            field(program_account.owner().as_ref());
            field(&[u8::from(program_account.executable())]);
            field(&last_modification_slot.to_le_bytes());
            field(program_account.data());
            field(&[u8::from(programdata.is_some())]);
            if let Some((address, account, modification_slot)) = programdata {
                field(address.as_ref());
                field(account.owner().as_ref());
                field(&[u8::from(account.executable())]);
                field(&modification_slot.to_le_bytes());
                field(account.data());
            }
            field(program_account.owner().as_ref());
            field(&(account_size as u64).to_le_bytes());
            field(&deployment_slot.to_le_bytes());
            field(&effective_slot.to_le_bytes());
            ProgramAccountIdentity(hasher.result().to_bytes())
        });
        if let (Some(cache), Some(identity)) = (program_load_cache, identity)
            && let Some(receipt) = cache.lookup(&identity, program_runtime_environment)
            && receipt.identity == identity
            && let Some(entry) = receipt.entry()
            && matches!(entry.program, ProgramCacheEntryType::Loaded(_))
            && entry.account_owner() == *program_account.owner()
            && entry.account_size == account_size
            && entry.deployment_slot == deployment_slot
            && entry.effective_slot == effective_slot
            && entry.program.get_environment() == Some(program_runtime_environment)
        {
            return Ok(entry);
        }
        let entry = Arc::new(ProgramCacheEntry::new(
            program_account.owner(),
            program_runtime_environment.clone(),
            deployment_slot,
            effective_slot,
            elf_bytes,
            account_size,
            #[cfg(feature = "metrics")]
            &mut load_program_metrics,
        )?);
        if let (Some(cache), Some(identity)) = (program_load_cache, identity) {
            cache.on_load(&VerifiedProgramReceipt {
                identity,
                entry: Arc::downgrade(&entry),
            });
        }
        Ok::<_, Box<dyn std::error::Error>>(entry)
    };
    let loaded_program = match load_result {
        ProgramAccountLoadResult::InvalidAccountData(owner) => Ok(Arc::new(
            ProgramCacheEntry::new_tombstone(current_slot, owner, ProgramCacheEntryType::Closed),
        )),

        ProgramAccountLoadResult::ProgramOfLoaderV1(program_account) => load(
            &program_account,
            None,
            0,
            program_account.data(),
            program_account.data().len(),
        )
        .map_err(|_| (0, ProgramCacheEntryOwner::LoaderV1)),

        ProgramAccountLoadResult::ProgramOfLoaderV2(program_account) => load(
            &program_account,
            None,
            0,
            program_account.data(),
            program_account.data().len(),
        )
        .map_err(|_| (0, ProgramCacheEntryOwner::LoaderV2)),

        ProgramAccountLoadResult::ProgramOfLoaderV3(
            program_account,
            programdata_account,
            deployment_slot,
            programdata_address,
            programdata_modification_slot,
        ) => programdata_account
            .data()
            .get(UpgradeableLoaderState::size_of_programdata_metadata()..)
            .ok_or(())
            .and_then(|programdata| {
                load(
                    &program_account,
                    Some((
                        &programdata_address,
                        &programdata_account,
                        programdata_modification_slot,
                    )),
                    deployment_slot,
                    programdata,
                    program_account
                        .data()
                        .len()
                        .saturating_add(programdata_account.data().len()),
                )
                .map_err(|_| ())
            })
            .map_err(|_| (deployment_slot, ProgramCacheEntryOwner::LoaderV3)),

        ProgramAccountLoadResult::ProgramOfLoaderV4(program_account, deployment_slot) => {
            program_account
                .data()
                .get(LoaderV4State::program_data_offset()..)
                .ok_or(())
                .and_then(|elf_bytes| {
                    load(
                        &program_account,
                        None,
                        deployment_slot,
                        elf_bytes,
                        program_account.data().len(),
                    )
                    .map_err(|_| ())
                })
                .map_err(|_| (deployment_slot, ProgramCacheEntryOwner::LoaderV4))
        }
    }
    .unwrap_or_else(|(deployment_slot, owner)| {
        let env = ProgramRuntimeEnvironment::clone(program_runtime_environment);
        Arc::new(ProgramCacheEntry::new_tombstone(
            deployment_slot,
            owner,
            ProgramCacheEntryType::FailedVerification(env),
        ))
    });

    #[cfg(feature = "metrics")]
    load_program_metrics.submit_datapoint(&mut execute_timings.details);
    loaded_program.update_access_slot(current_slot);
    Some((loaded_program, last_modification_slot))
}

/// Find the slot in which the program was most recently re-/deployed.
/// Returns slot 0 for programs deployed with v1/v2 loaders, since programs deployed
/// with those loaders do not retain deployment slot information.
/// Returns an error if the program's account state can not be found or parsed.
pub(crate) fn get_program_deployment_slot<CB: TransactionProcessingCallback>(
    callbacks: &CB,
    program: &AccountSharedData,
    loader: ProgramCacheEntryOwner,
) -> TransactionResult<Slot> {
    match loader {
        ProgramCacheEntryOwner::LoaderV1 | ProgramCacheEntryOwner::LoaderV2 => Ok(0),
        ProgramCacheEntryOwner::LoaderV3 => {
            if let Ok(UpgradeableLoaderState::Program {
                programdata_address,
            }) = program.state()
            {
                let (programdata, _slot) = callbacks
                    .get_account_shared_data(&programdata_address)
                    .ok_or(TransactionError::ProgramAccountNotFound)?;
                if let Ok(UpgradeableLoaderState::ProgramData {
                    slot,
                    upgrade_authority_address: _,
                }) = programdata.state()
                {
                    return Ok(slot);
                }
            }
            Err(TransactionError::ProgramAccountNotFound)
        }
        ProgramCacheEntryOwner::LoaderV4 => {
            let state = loader_v4_get_state(program.data())
                .map_err(|_| TransactionError::ProgramAccountNotFound)?;
            Ok(state.slot)
        }
        ProgramCacheEntryOwner::NativeLoader => unreachable!(),
    }
}

/// Appends to a set of executable program accounts (all accounts owned by any loader)
/// for transactions with a valid blockhash or nonce.
pub fn filter_executable_program_accounts<'a, CB: TransactionProcessingCallback>(
    callbacks: &CB,
    program_cache_for_tx_batch: &ProgramCacheForTxBatch,
    keys: impl Iterator<Item = &'a Pubkey>,
    check_program_deployment_slot: bool,
) -> Vec<ProgramToLoad<'a>> {
    let mut result = Vec::new();
    for account_key in keys {
        if let Some(cache_entry) = program_cache_for_tx_batch.find(account_key) {
            cache_entry.stats.uses.fetch_add(1, Ordering::Relaxed);
        } else if let Some((account, last_modification_slot)) =
            callbacks.get_account_shared_data(account_key)
        {
            let loader = if loader_v4::check_id(account.owner()) {
                ProgramCacheEntryOwner::LoaderV4
            } else if bpf_loader_upgradeable::check_id(account.owner()) {
                ProgramCacheEntryOwner::LoaderV3
            } else if bpf_loader::check_id(account.owner()) {
                ProgramCacheEntryOwner::LoaderV2
            } else if bpf_loader_deprecated::check_id(account.owner()) {
                ProgramCacheEntryOwner::LoaderV1
            } else {
                continue;
            };
            let match_criteria = if check_program_deployment_slot {
                get_program_deployment_slot(callbacks, &account, loader)
                    .map_or(ProgramCacheMatchCriteria::Tombstone, |slot| {
                        ProgramCacheMatchCriteria::DeployedOnOrAfterSlot(slot)
                    })
            } else {
                ProgramCacheMatchCriteria::NoCriteria
            };
            result.push(ProgramToLoad {
                program_id: account_key,
                loader,
                match_criteria,
                last_modification_slot,
            });
        }
    }
    result
}

// Plucked from the now-removed Loader V4 program library.
fn loader_v4_get_state(data: &[u8]) -> Result<&LoaderV4State, InstructionError> {
    unsafe {
        let data = data
            .get(0..LoaderV4State::program_data_offset())
            .ok_or(InstructionError::AccountDataTooSmall)?
            .try_into()
            .unwrap();
        Ok(std::mem::transmute::<
            &[u8; LoaderV4State::program_data_offset()],
            &LoaderV4State,
        >(data))
    }
}

#[cfg(test)]
mod tests {
    use {
        super::*,
        crate::transaction_processor::TransactionBatchProcessor,
        solana_account::WritableAccount,
        solana_hash::Hash,
        solana_keypair::Keypair,
        solana_message::compiled_instruction::CompiledInstruction,
        solana_program_runtime::{
            loaded_programs::{
                BlockRelation, ForkGraph, ProgramRuntimeEnvironment,
                get_mock_program_runtime_environment,
            },
            solana_sbpf::program::BuiltinProgram,
        },
        solana_sdk_ids::{bpf_loader, bpf_loader_upgradeable, native_loader},
        solana_svm_transaction::svm_message::SVMMessage,
        solana_svm_type_overrides::sync::atomic::AtomicU64,
        solana_transaction::{Transaction, sanitized::SanitizedTransaction},
        std::{
            cell::RefCell,
            collections::HashMap,
            env,
            fs::{self, File},
            io::Read,
        },
    };

    struct TestForkGraph {}

    impl ForkGraph for TestForkGraph {
        fn relationship(&self, _a: Slot, _b: Slot) -> BlockRelation {
            BlockRelation::Unknown
        }
    }

    #[derive(Default, Clone)]
    pub(crate) struct MockBankCallback {
        pub(crate) account_shared_data: RefCell<HashMap<Pubkey, (AccountSharedData, Slot)>>,
    }

    impl TransactionProcessingCallback for MockBankCallback {
        fn get_account_shared_data(&self, pubkey: &Pubkey) -> Option<(AccountSharedData, Slot)> {
            self.account_shared_data.borrow().get(pubkey).cloned()
        }
    }

    #[derive(Default)]
    struct ReceiptCache {
        receipt: std::sync::Mutex<Option<VerifiedProgramReceipt>>,
        lookups: AtomicU64,
        loads: AtomicU64,
    }

    impl ProgramLoadCache for ReceiptCache {
        fn lookup(
            &self,
            _: &ProgramAccountIdentity,
            _: &ProgramRuntimeEnvironment,
        ) -> Option<VerifiedProgramReceipt> {
            self.lookups.fetch_add(1, Ordering::Relaxed);
            self.receipt.lock().unwrap().clone()
        }

        fn on_load(&self, receipt: &VerifiedProgramReceipt) {
            self.loads.fetch_add(1, Ordering::Relaxed);
            *self.receipt.lock().unwrap() = Some(receipt.clone());
        }
    }

    #[test]
    fn test_verified_receipt_reuse_and_cold_misses() {
        assert!(
            crate::transaction_processor::TransactionProcessingConfig::default()
                .program_load_cache
                .is_none()
        );
        for owner in [bpf_loader_deprecated::id(), bpf_loader::id()] {
            let bank = MockBankCallback::default();
            let key = Pubkey::new_unique();
            let mut account = AccountSharedData::new(1, 0, &owner);
            account.set_data(load_test_program());
            bank.account_shared_data
                .borrow_mut()
                .insert(key, (account.clone(), 7));
            let env = get_mock_program_runtime_environment();
            let cache = ReceiptCache::default();
            let load =
                |env: &ProgramRuntimeEnvironment, slot, hook, timings: &mut ExecuteTimings| {
                    load_program_with_pubkey_and_cache(&bank, env, &key, slot, timings, hook)
                        .unwrap()
                };
            let (baseline, _) =
                load_program_with_pubkey(&bank, &env, &key, 8, &mut ExecuteTimings::default())
                    .unwrap();
            let (original, modification_slot) =
                load(&env, 8, Some(&cache), &mut ExecuteTimings::default());
            assert_eq!(modification_slot, 7);
            assert_eq!(original, baseline);
            assert!(!Arc::ptr_eq(&original, &baseline));
            let receipt = cache.receipt.lock().unwrap().clone().unwrap();
            assert!(Arc::ptr_eq(&original, &receipt.entry().unwrap()));
            original.stats.jit_compiled(123);
            let compilations = original.stats.compilations.load(Ordering::Relaxed);
            let ema = original.stats.compilation_time_ema.load(Ordering::Relaxed);
            let mut timings = ExecuteTimings::default();
            let (recovered, _) = load(&env, 9, Some(&cache), &mut timings);
            assert!(Arc::ptr_eq(&original, &recovered));
            assert_eq!(cache.loads.load(Ordering::Relaxed), 1);
            assert_eq!(original.latest_access_slot.load(Ordering::Relaxed), 9);
            assert_eq!(
                original.stats.compilations.load(Ordering::Relaxed),
                compilations
            );
            assert_eq!(
                original.stats.compilation_time_ema.load(Ordering::Relaxed),
                ema
            );
            assert_eq!(timings.details.create_executor_load_elf_us.0, 0);
            assert_eq!(timings.details.create_executor_verify_code_us.0, 0);
            assert_eq!(timings.details.create_executor_jit_compile_us.0, 0);
            let mut delayed = ProgramCacheForTxBatch::new(0);
            delayed.replenish(key, recovered.clone());
            assert!(matches!(
                delayed.find(&key).unwrap().program,
                ProgramCacheEntryType::DelayVisibility
            ));

            // Same deployment slot, different full ELF bytes (including ignored trailing bytes).
            for change in 0..6 {
                let mut changed = account.clone();
                let mut slot = 7;
                match change {
                    0 => {
                        let mut bytes = changed.data().to_vec();
                        bytes.push(0);
                        changed.set_data(bytes);
                    }
                    1 => changed.set_owner(if owner == bpf_loader::id() {
                        bpf_loader_deprecated::id()
                    } else {
                        bpf_loader::id()
                    }),
                    2 => slot = 8,
                    3 => changed.set_executable(true),
                    5 => changed.set_data(
                        fs::read(
                            "tests/example-programs/simple-transfer/simple_transfer_program.so",
                        )
                        .unwrap(),
                    ),
                    _ => {
                        let mut bytes = changed.data().to_vec();
                        bytes[0] = 0;
                        changed.set_data(bytes);
                    }
                }
                *cache.receipt.lock().unwrap() = Some(receipt.clone());
                bank.account_shared_data
                    .borrow_mut()
                    .insert(key, (changed, slot));
                let (cold, returned_slot) =
                    load(&env, 9, Some(&cache), &mut ExecuteTimings::default());
                assert!(!Arc::ptr_eq(&original, &cold));
                assert_eq!(returned_slot, slot);
                assert_eq!(
                    matches!(cold.program, ProgramCacheEntryType::FailedVerification(_)),
                    change == 4
                );
            }
            bank.account_shared_data
                .borrow_mut()
                .insert(key, (account, 7));
            *cache.receipt.lock().unwrap() = Some(receipt.clone());
            let different_env = ProgramRuntimeEnvironment::from(BuiltinProgram::new_mock());
            let (cold, _) = load(
                &different_env,
                9,
                Some(&cache),
                &mut ExecuteTimings::default(),
            );
            assert!(!Arc::ptr_eq(&cold, &original));
            assert_eq!(cold.program.get_environment(), Some(&different_env));
            drop(delayed);
            drop(recovered);
            drop(original);
            assert!(receipt.entry().is_none());
            *cache.receipt.lock().unwrap() = Some(receipt);
            assert!(matches!(
                load(&env, 9, Some(&cache), &mut ExecuteTimings::default())
                    .0
                    .program,
                ProgramCacheEntryType::Loaded(_)
            ));
        }
    }

    #[test]
    fn test_verified_receipt_v3_resolved_identity_and_read_order() {
        struct RecordingBank {
            bank: MockBankCallback,
            reads: RefCell<Vec<Pubkey>>,
        }
        impl TransactionProcessingCallback for RecordingBank {
            fn get_account_shared_data(&self, key: &Pubkey) -> Option<(AccountSharedData, Slot)> {
                self.reads.borrow_mut().push(*key);
                self.bank.get_account_shared_data(key)
            }
        }
        let bank = RecordingBank {
            bank: MockBankCallback::default(),
            reads: RefCell::default(),
        };
        let key = Pubkey::new_unique();
        let data_key = Pubkey::new_unique();
        let program = AccountSharedData::new_data(
            1,
            &UpgradeableLoaderState::Program {
                programdata_address: data_key,
            },
            &bpf_loader_upgradeable::id(),
        )
        .unwrap();
        let mut bytes = bincode::serialize(&UpgradeableLoaderState::ProgramData {
            slot: 5,
            upgrade_authority_address: None,
        })
        .unwrap();
        bytes.resize(UpgradeableLoaderState::size_of_programdata_metadata(), 0);
        bytes.extend(load_test_program());
        let mut data = AccountSharedData::new(1, 0, &bpf_loader_upgradeable::id());
        data.set_data(bytes);
        let env = get_mock_program_runtime_environment();
        let cache = ReceiptCache::default();
        let load = |hook| {
            load_program_with_pubkey_and_cache(
                &bank,
                &env,
                &key,
                6,
                &mut ExecuteTimings::default(),
                hook,
            )
        };
        bank.bank
            .account_shared_data
            .borrow_mut()
            .extend([(key, (program.clone(), 10)), (data_key, (data.clone(), 11))]);
        let baseline = load(None).unwrap().0;
        assert_eq!(*bank.reads.borrow(), [key, data_key]);
        bank.reads.borrow_mut().clear();
        let original = load(Some(&cache)).unwrap().0;
        assert_eq!(*bank.reads.borrow(), [key, data_key]);
        assert_eq!(baseline, original);
        assert_eq!(
            original.account_size,
            program.data().len() + data.data().len()
        );
        let receipt = cache.receipt.lock().unwrap().clone().unwrap();
        bank.reads.borrow_mut().clear();
        assert!(Arc::ptr_eq(&original, &load(Some(&cache)).unwrap().0));
        assert_eq!(*bank.reads.borrow(), [key, data_key]);
        for change in 0..10 {
            let mut p = program.clone();
            let mut d = data.clone();
            let mut address = data_key;
            let mut modification_slot = 11;
            let mut bytes = d.data().to_vec();
            match change {
                0 => bytes[13] = 1, // authority padding is part of the source
                1 => {
                    bytes[12] = 1;
                    bytes[13..45].copy_from_slice(Pubkey::new_unique().as_ref());
                }
                2 => {
                    address = Pubkey::new_unique();
                    p.set_data(
                        bincode::serialize(&UpgradeableLoaderState::Program {
                            programdata_address: address,
                        })
                        .unwrap(),
                    );
                }
                3 => modification_slot = 12,
                4 => bytes[4..12].copy_from_slice(&6u64.to_le_bytes()),
                5 => bytes.push(0),
                6 => d.set_owner(bpf_loader::id()),
                7 => bytes.truncate(13), // parsed metadata, failed ELF slice
                9 => {
                    bytes.truncate(UpgradeableLoaderState::size_of_programdata_metadata());
                    bytes.extend(
                        fs::read(
                            "tests/example-programs/simple-transfer/simple_transfer_program.so",
                        )
                        .unwrap(),
                    );
                }
                _ => {
                    p.set_data(bincode::serialize(&UpgradeableLoaderState::Uninitialized).unwrap())
                }
            }
            d.set_data(bytes);
            bank.bank
                .account_shared_data
                .borrow_mut()
                .extend([(key, (p, 10)), (address, (d, modification_slot))]);
            *cache.receipt.lock().unwrap() = Some(receipt.clone());
            let lookups = cache.lookups.load(Ordering::Relaxed);
            let cold = load(Some(&cache)).unwrap().0;
            assert!(!Arc::ptr_eq(&original, &cold), "change {change}");
            if (6..9).contains(&change) {
                assert_eq!(cache.lookups.load(Ordering::Relaxed), lookups);
                assert!(cold.is_tombstone());
                if change == 7 {
                    assert!(
                        matches!(&cold.program, ProgramCacheEntryType::FailedVerification(failed_env) if failed_env == &env)
                    );
                }
            } else {
                assert!(matches!(cold.program, ProgramCacheEntryType::Loaded(_)));
                assert_ne!(
                    cache.receipt.lock().unwrap().as_ref().unwrap().identity(),
                    receipt.identity()
                );
            }
        }
        bank.bank
            .account_shared_data
            .borrow_mut()
            .insert(key, (program, 10));
        bank.bank.account_shared_data.borrow_mut().remove(&data_key);
        let lookups = cache.lookups.load(Ordering::Relaxed);
        assert!(matches!(
            load(Some(&cache)).unwrap().0.program,
            ProgramCacheEntryType::Closed
        ));
        bank.bank.account_shared_data.borrow_mut().remove(&key);
        assert!(load(Some(&cache)).is_none());
        assert_eq!(cache.lookups.load(Ordering::Relaxed), lookups);
    }

    #[test]
    fn test_verified_receipt_v4_retracted_and_invalid() {
        let bank = MockBankCallback::default();
        let key = Pubkey::new_unique();
        let env = get_mock_program_runtime_environment();
        let cache = ReceiptCache::default();
        let mut data = vec![0; LoaderV4State::program_data_offset()];
        data[..8].copy_from_slice(&5u64.to_le_bytes());
        data[40..48].copy_from_slice(&(LoaderV4Status::Deployed as u64).to_le_bytes());
        data.extend(load_test_program());
        let mut account = AccountSharedData::new(1, 0, &loader_v4::id());
        account.set_data(data.clone());
        bank.account_shared_data
            .borrow_mut()
            .insert(key, (account.clone(), 7));
        let load = || {
            load_program_with_pubkey_and_cache(
                &bank,
                &env,
                &key,
                5,
                &mut ExecuteTimings::default(),
                Some(&cache),
            )
            .unwrap()
            .0
        };
        let original = load();
        assert!(matches!(original.program, ProgramCacheEntryType::Loaded(_)));
        assert!(Arc::ptr_eq(&original, &load()));
        for retracted in [true, false] {
            if retracted {
                data[40..48].copy_from_slice(&(LoaderV4Status::Retracted as u64).to_le_bytes());
            } else {
                data.truncate(1);
            }
            account.set_data(data.clone());
            bank.account_shared_data
                .borrow_mut()
                .insert(key, (account.clone(), 7));
            let lookups = cache.lookups.load(Ordering::Relaxed);
            assert!(matches!(load().program, ProgramCacheEntryType::Closed));
            assert_eq!(cache.lookups.load(Ordering::Relaxed), lookups);
        }
    }

    #[test]
    fn test_load_program_accounts_account_not_found() {
        let mock_bank = MockBankCallback::default();
        let key = Pubkey::new_unique();

        let result = load_program_accounts(&mock_bank, &key);
        assert!(result.is_none());

        let mut account_data = AccountSharedData::default();
        account_data.set_owner(bpf_loader_upgradeable::id());
        let state = UpgradeableLoaderState::Program {
            programdata_address: Pubkey::new_unique(),
        };
        account_data.set_data(bincode::serialize(&state).unwrap());
        mock_bank
            .account_shared_data
            .borrow_mut()
            .insert(key, (account_data.clone(), 0));

        let result = load_program_accounts(&mock_bank, &key);
        assert!(matches!(
            result,
            Some((ProgramAccountLoadResult::InvalidAccountData(_), _))
        ));

        account_data.set_data(Vec::new());
        mock_bank
            .account_shared_data
            .borrow_mut()
            .insert(key, (account_data, 0));

        let result = load_program_accounts(&mock_bank, &key);

        assert!(matches!(
            result,
            Some((ProgramAccountLoadResult::InvalidAccountData(_), _))
        ));
    }

    #[test]
    fn test_load_program_accounts_loader_v1_or_v2() {
        let key = Pubkey::new_unique();
        let mock_bank = MockBankCallback::default();
        let mut account_data = AccountSharedData::default();
        account_data.set_owner(bpf_loader::id());
        mock_bank
            .account_shared_data
            .borrow_mut()
            .insert(key, (account_data.clone(), 0));

        let result = load_program_accounts(&mock_bank, &key);
        match result {
            Some((ProgramAccountLoadResult::ProgramOfLoaderV1(data), last_modification_slot))
            | Some((ProgramAccountLoadResult::ProgramOfLoaderV2(data), last_modification_slot)) => {
                assert_eq!(data, account_data);
                assert_eq!(last_modification_slot, 0);
            }
            _ => panic!("Invalid result"),
        }
    }

    #[test]
    fn test_load_program_accounts_success() {
        let key1 = Pubkey::new_unique();
        let key2 = Pubkey::new_unique();
        let mock_bank = MockBankCallback::default();

        let mut account_data = AccountSharedData::default();
        account_data.set_owner(bpf_loader_upgradeable::id());

        let state = UpgradeableLoaderState::Program {
            programdata_address: key2,
        };
        account_data.set_data(bincode::serialize(&state).unwrap());
        mock_bank
            .account_shared_data
            .borrow_mut()
            .insert(key1, (account_data.clone(), 25));

        let state = UpgradeableLoaderState::ProgramData {
            slot: 25,
            upgrade_authority_address: None,
        };
        let mut account_data2 = AccountSharedData::default();
        account_data2.set_owner(bpf_loader_upgradeable::id());
        account_data2.set_data(bincode::serialize(&state).unwrap());
        mock_bank
            .account_shared_data
            .borrow_mut()
            .insert(key2, (account_data2.clone(), 25));

        let result = load_program_accounts(&mock_bank, &key1);

        match result {
            Some((
                ProgramAccountLoadResult::ProgramOfLoaderV3(
                    data1,
                    data2,
                    deployment_slot,
                    address,
                    modification_slot,
                ),
                last_modification_slot,
            )) => {
                assert_eq!(data1, account_data);
                assert_eq!(data2, account_data2);
                assert_eq!(deployment_slot, 25);
                assert_eq!(address, key2);
                assert_eq!(modification_slot, 25);
                assert_eq!(last_modification_slot, 25);
            }

            _ => panic!("Invalid result"),
        }
    }

    fn load_test_program() -> Vec<u8> {
        let mut dir = env::current_dir().unwrap();
        dir.push("tests");
        dir.push("example-programs");
        dir.push("hello-solana");
        dir.push("hello_solana_program.so");
        let mut file = File::open(dir.clone()).expect("file not found");
        let metadata = fs::metadata(dir).expect("Unable to read metadata");
        let mut buffer = vec![0; metadata.len() as usize];
        file.read_exact(&mut buffer).expect("Buffer overflow");
        buffer
    }

    #[test]
    fn test_load_program_from_bytes() {
        let buffer = load_test_program();

        #[cfg(feature = "metrics")]
        let mut metrics = LoadProgramMetrics::default();
        let loader = bpf_loader_upgradeable::id();
        let size = buffer.len();
        let slot: Slot = 2;
        let environment = ProgramRuntimeEnvironment::from(BuiltinProgram::new_mock());

        let result = ProgramCacheEntry::new(
            &loader,
            ProgramRuntimeEnvironment::clone(&environment),
            slot,
            slot.saturating_add(DELAY_VISIBILITY_SLOT_OFFSET),
            &buffer,
            size,
            #[cfg(feature = "metrics")]
            &mut metrics,
        );

        assert!(result.is_ok());
    }

    #[test]
    fn test_load_program_not_found() {
        let mock_bank = MockBankCallback::default();
        let key = Pubkey::new_unique();
        let batch_processor = TransactionBatchProcessor::<TestForkGraph>::default();

        let result = load_program_with_pubkey(
            &mock_bank,
            &batch_processor.program_runtime_environment_for_epoch(50),
            &key,
            500,
            &mut ExecuteTimings::default(),
        );
        assert!(result.is_none());
    }

    #[test]
    fn test_load_program_invalid_account_data() {
        let key = Pubkey::new_unique();
        let mock_bank = MockBankCallback::default();
        let mut account_data = AccountSharedData::default();
        account_data.set_owner(bpf_loader_upgradeable::id());
        let batch_processor = TransactionBatchProcessor::<TestForkGraph>::default();
        mock_bank
            .account_shared_data
            .borrow_mut()
            .insert(key, (account_data.clone(), 0));

        let result = load_program_with_pubkey(
            &mock_bank,
            &batch_processor.program_runtime_environment_for_epoch(20),
            &key,
            0, // Slot 0
            &mut ExecuteTimings::default(),
        );

        let loaded_program = ProgramCacheEntry::new_tombstone(
            0, // Slot 0
            ProgramCacheEntryOwner::LoaderV3,
            ProgramCacheEntryType::FailedVerification(
                batch_processor.program_runtime_environment_for_epoch(20),
            ),
        );
        assert_eq!(result.unwrap(), (Arc::new(loaded_program), 0));
    }

    #[test]
    fn test_load_program_program_loader_v1_or_v2() {
        let key = Pubkey::new_unique();
        let mock_bank = MockBankCallback::default();
        let mut account_data = AccountSharedData::default();
        account_data.set_owner(bpf_loader::id());
        let batch_processor = TransactionBatchProcessor::<TestForkGraph>::default();
        mock_bank
            .account_shared_data
            .borrow_mut()
            .insert(key, (account_data.clone(), 0));

        // This should return an error
        let result = load_program_with_pubkey(
            &mock_bank,
            &batch_processor.program_runtime_environment_for_epoch(20),
            &key,
            200,
            &mut ExecuteTimings::default(),
        );
        let loaded_program = ProgramCacheEntry::new_tombstone(
            0,
            ProgramCacheEntryOwner::LoaderV2,
            ProgramCacheEntryType::FailedVerification(
                batch_processor.program_runtime_environment_for_epoch(20),
            ),
        );
        assert_eq!(result.unwrap(), (Arc::new(loaded_program), 0));

        let buffer = load_test_program();
        account_data.set_data(buffer);

        mock_bank
            .account_shared_data
            .borrow_mut()
            .insert(key, (account_data.clone(), 0));

        let result = load_program_with_pubkey(
            &mock_bank,
            &batch_processor.program_runtime_environment_for_epoch(20),
            &key,
            200,
            &mut ExecuteTimings::default(),
        );

        let program_runtime_environment = get_mock_program_runtime_environment();
        let expected = ProgramCacheEntry::new(
            account_data.owner(),
            ProgramRuntimeEnvironment::clone(&program_runtime_environment),
            0,
            DELAY_VISIBILITY_SLOT_OFFSET,
            account_data.data(),
            account_data.data().len(),
            #[cfg(feature = "metrics")]
            &mut LoadProgramMetrics::default(),
        );

        assert_eq!(result.unwrap(), (Arc::new(expected.unwrap()), 0));
    }

    #[test]
    fn test_load_program_program_loader_v3() {
        let key1 = Pubkey::new_unique();
        let key2 = Pubkey::new_unique();
        let mock_bank = MockBankCallback::default();
        let batch_processor = TransactionBatchProcessor::<TestForkGraph>::default();

        let mut account_data = AccountSharedData::default();
        account_data.set_owner(bpf_loader_upgradeable::id());

        let state = UpgradeableLoaderState::Program {
            programdata_address: key2,
        };
        account_data.set_data(bincode::serialize(&state).unwrap());
        mock_bank
            .account_shared_data
            .borrow_mut()
            .insert(key1, (account_data.clone(), 0));

        let state = UpgradeableLoaderState::ProgramData {
            slot: 0,
            upgrade_authority_address: None,
        };
        let mut account_data2 = AccountSharedData::default();
        account_data2.set_data(bincode::serialize(&state).unwrap());
        mock_bank
            .account_shared_data
            .borrow_mut()
            .insert(key2, (account_data2.clone(), 0));

        // This should return an error
        let result = load_program_with_pubkey(
            &mock_bank,
            &batch_processor.program_runtime_environment_for_epoch(0),
            &key1,
            0,
            &mut ExecuteTimings::default(),
        );
        let loaded_program = ProgramCacheEntry::new_tombstone(
            0,
            ProgramCacheEntryOwner::LoaderV3,
            ProgramCacheEntryType::FailedVerification(
                batch_processor.program_runtime_environment_for_epoch(0),
            ),
        );
        assert_eq!(result.unwrap(), (Arc::new(loaded_program), 0));

        let mut buffer = load_test_program();
        let mut header = bincode::serialize(&state).unwrap();
        let mut complement = vec![
            0;
            std::cmp::max(
                0,
                UpgradeableLoaderState::size_of_programdata_metadata() - header.len()
            )
        ];
        header.append(&mut complement);
        header.append(&mut buffer);
        account_data.set_data(header);

        mock_bank
            .account_shared_data
            .borrow_mut()
            .insert(key2, (account_data.clone(), 0));

        let result = load_program_with_pubkey(
            &mock_bank,
            &batch_processor.program_runtime_environment_for_epoch(20),
            &key1,
            200,
            &mut ExecuteTimings::default(),
        );

        let data = account_data.data();
        account_data
            .set_data(data[UpgradeableLoaderState::size_of_programdata_metadata()..].to_vec());

        let program_runtime_environment = get_mock_program_runtime_environment();
        let expected = ProgramCacheEntry::new(
            account_data.owner(),
            ProgramRuntimeEnvironment::clone(&program_runtime_environment),
            0,
            DELAY_VISIBILITY_SLOT_OFFSET,
            account_data.data(),
            account_data.data().len(),
            #[cfg(feature = "metrics")]
            &mut LoadProgramMetrics::default(),
        );
        assert_eq!(result.unwrap(), (Arc::new(expected.unwrap()), 0));
    }

    #[test]
    fn test_load_program_environment() {
        let key = Pubkey::new_unique();
        let mock_bank = MockBankCallback::default();
        let mut account_data = AccountSharedData::default();
        account_data.set_owner(bpf_loader::id());
        let batch_processor = TransactionBatchProcessor::<TestForkGraph>::default();
        let upcoming_environment = get_mock_program_runtime_environment();
        let current_environment =
            ProgramRuntimeEnvironment::clone(&batch_processor.program_runtime_environment);
        {
            let mut epoch_boundary_preparation =
                batch_processor.epoch_boundary_preparation.write().unwrap();
            epoch_boundary_preparation.upcoming_epoch = 1;
            epoch_boundary_preparation.upcoming_environment = Some(upcoming_environment.clone());
        }
        mock_bank
            .account_shared_data
            .borrow_mut()
            .insert(key, (account_data.clone(), 0));

        for is_upcoming_env in [false, true] {
            let (result, _last_modification_slot) = load_program_with_pubkey(
                &mock_bank,
                &batch_processor.program_runtime_environment_for_epoch(is_upcoming_env as u64),
                &key,
                200,
                &mut ExecuteTimings::default(),
            )
            .unwrap();
            assert_ne!(
                is_upcoming_env,
                result.program.get_environment().unwrap() == &current_environment,
            );
            assert_eq!(
                is_upcoming_env,
                result.program.get_environment().unwrap() == &upcoming_environment,
            );
        }
    }

    #[test]
    fn test_program_modification_slot_account_not_found() {
        let mock_bank = MockBankCallback::default();
        let key = Pubkey::new_unique();

        let mut account_data = AccountSharedData::new(100, 100, &bpf_loader_upgradeable::id());
        mock_bank
            .account_shared_data
            .borrow_mut()
            .insert(key, (account_data.clone(), 0));

        let result = get_program_deployment_slot(
            &mock_bank,
            &mock_bank.get_account_shared_data(&key).unwrap().0,
            ProgramCacheEntryOwner::LoaderV3,
        );
        assert_eq!(result.err(), Some(TransactionError::ProgramAccountNotFound));

        let state = UpgradeableLoaderState::Program {
            programdata_address: Pubkey::new_unique(),
        };
        account_data.set_data(bincode::serialize(&state).unwrap());
        mock_bank
            .account_shared_data
            .borrow_mut()
            .insert(key, (account_data.clone(), 0));

        let result = get_program_deployment_slot(
            &mock_bank,
            &mock_bank.get_account_shared_data(&key).unwrap().0,
            ProgramCacheEntryOwner::LoaderV3,
        );
        assert_eq!(result.err(), Some(TransactionError::ProgramAccountNotFound));
    }

    #[test]
    fn test_program_deployment_slot_success() {
        let mock_bank = MockBankCallback::default();

        let key1 = Pubkey::new_unique();
        let key2 = Pubkey::new_unique();

        let account_data = AccountSharedData::new_data(
            100,
            &UpgradeableLoaderState::Program {
                programdata_address: key2,
            },
            &bpf_loader_upgradeable::id(),
        )
        .unwrap();
        mock_bank
            .account_shared_data
            .borrow_mut()
            .insert(key1, (account_data, 0));

        let account_data = AccountSharedData::new_data(
            100,
            &UpgradeableLoaderState::ProgramData {
                slot: 77,
                upgrade_authority_address: None,
            },
            &bpf_loader_upgradeable::id(),
        )
        .unwrap();
        mock_bank
            .account_shared_data
            .borrow_mut()
            .insert(key2, (account_data.clone(), 0));

        let result = get_program_deployment_slot(
            &mock_bank,
            &mock_bank.get_account_shared_data(&key1).unwrap().0,
            ProgramCacheEntryOwner::LoaderV3,
        );
        assert_eq!(result.unwrap(), 77);
    }

    #[test]
    fn test_filter_executable_program_accounts() {
        let feepayer = Keypair::new();
        let loader_ids = [
            bpf_loader_deprecated::id(),
            bpf_loader::id(),
            bpf_loader_upgradeable::id(),
            native_loader::id(),
        ];
        let program_ids = [
            Pubkey::new_unique(),
            Pubkey::new_unique(),
            Pubkey::new_unique(),
            Pubkey::new_unique(),
        ];
        let account_ids = [
            Pubkey::new_unique(),
            Pubkey::new_unique(),
            Pubkey::new_unique(),
            Pubkey::new_unique(),
        ];

        let mut loaded_programs_for_tx_batch = ProgramCacheForTxBatch::default();
        let mock_bank = MockBankCallback::default();
        for i in 0..3 {
            loaded_programs_for_tx_batch.replenish(
                loader_ids[i],
                Arc::new(ProgramCacheEntry {
                    program: ProgramCacheEntryType::Builtin(BuiltinProgram::new_mock()),
                    account_owner: ProgramCacheEntryOwner::NativeLoader,
                    account_size: 0,
                    deployment_slot: 0,
                    effective_slot: 0,
                    stats: Arc::default(),
                    latest_access_slot: AtomicU64::default(),
                }),
            );
            mock_bank.account_shared_data.borrow_mut().insert(
                loader_ids[i],
                (AccountSharedData::new(1, 1, &program_ids[3]), 0),
            );
            mock_bank.account_shared_data.borrow_mut().insert(
                program_ids[i],
                (AccountSharedData::new(1, 1, &loader_ids[i]), 0),
            );
            mock_bank.account_shared_data.borrow_mut().insert(
                account_ids[i],
                (AccountSharedData::new(1, 1, &program_ids[i]), 0),
            );
        }

        let tx = Transaction::new_with_compiled_instructions(
            &[&feepayer],
            &[program_ids[1], program_ids[2], loader_ids[2]],
            Hash::new_unique(),
            vec![
                account_ids[0],
                account_ids[1],
                account_ids[2],
                account_ids[3],
            ],
            vec![
                CompiledInstruction::new(1, &(), vec![0, 1, 2, 3]),
                CompiledInstruction::new(2, &(), vec![0, 1, 2, 3]),
                CompiledInstruction::new(3, &(), vec![0, 1, 2, 3]),
            ],
        );
        let sanitized_tx = SanitizedTransaction::from_transaction_for_tests(tx);

        let missing_programs = filter_executable_program_accounts(
            &mock_bank,
            &loaded_programs_for_tx_batch,
            sanitized_tx.account_keys().iter(),
            false,
        );
        assert_eq!(
            missing_programs,
            &[
                ProgramToLoad {
                    program_id: &program_ids[1],
                    loader: ProgramCacheEntryOwner::LoaderV2,
                    match_criteria: ProgramCacheMatchCriteria::NoCriteria,
                    last_modification_slot: 0,
                },
                ProgramToLoad {
                    program_id: &program_ids[2],
                    loader: ProgramCacheEntryOwner::LoaderV3,
                    match_criteria: ProgramCacheMatchCriteria::NoCriteria,
                    last_modification_slot: 0,
                },
            ]
        );

        let missing_programs = filter_executable_program_accounts(
            &mock_bank,
            &loaded_programs_for_tx_batch,
            sanitized_tx.account_keys().iter(),
            true,
        );
        assert_eq!(
            missing_programs,
            &[
                ProgramToLoad {
                    program_id: &program_ids[1],
                    loader: ProgramCacheEntryOwner::LoaderV2,
                    match_criteria: ProgramCacheMatchCriteria::DeployedOnOrAfterSlot(0),
                    last_modification_slot: 0,
                },
                ProgramToLoad {
                    program_id: &program_ids[2],
                    loader: ProgramCacheEntryOwner::LoaderV3,
                    match_criteria: ProgramCacheMatchCriteria::Tombstone,
                    last_modification_slot: 0,
                },
            ]
        );
    }
}
