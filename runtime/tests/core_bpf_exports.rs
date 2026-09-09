#![cfg(feature = "agave-unstable-api")]

use {
    solana_account::{AccountSharedData, ReadableAccount, WritableAccount},
    solana_builtins::core_bpf_migration::CoreBpfMigrationTargetType,
    solana_loader_v3_interface::{get_program_data_address, state::UpgradeableLoaderState},
    solana_pubkey::Pubkey,
    solana_rent::Rent,
    solana_runtime::core_bpf_migration::{
        AccountReader, CoreBpfMigrationError as Error, SourceBuffer, TargetBpfV2, TargetBuiltin,
        TargetCoreBpf, checked_add, new_target_program_account, new_target_program_data_account,
    },
    solana_sdk_ids::{bpf_loader, bpf_loader_upgradeable, native_loader, system_program},
    std::{cell::RefCell, collections::HashMap},
};

#[derive(Default)]
struct Reader {
    accounts: HashMap<Pubkey, AccountSharedData>,
    reads: RefCell<Vec<Pubkey>>,
}

impl AccountReader for Reader {
    fn read(&self, key: &Pubkey) -> Option<AccountSharedData> {
        self.reads.borrow_mut().push(*key);
        self.accounts.get(key).cloned()
    }
}

fn buffer(authority_address: Option<Pubkey>, elf: &[u8]) -> AccountSharedData {
    let offset = UpgradeableLoaderState::size_of_buffer_metadata();
    let mut account = AccountSharedData::new_data_with_space(
        91,
        &UpgradeableLoaderState::Buffer { authority_address },
        offset + elf.len(),
        &bpf_loader_upgradeable::id(),
    )
    .unwrap();
    account.data_as_mut_slice()[offset..].copy_from_slice(elf);
    account
}

#[test]
fn source_checks_and_build_hash_preserve_error_order() {
    let address = Pubkey::new_unique();
    let mut reader = Reader::default();
    assert!(
        matches!(SourceBuffer::new_checked(&reader, &address), Err(Error::AccountNotFound(k)) if k == address)
    );
    reader
        .accounts
        .insert(address, AccountSharedData::default());
    assert!(
        matches!(SourceBuffer::new_checked(&reader, &address), Err(Error::IncorrectOwner(k)) if k == address)
    );
    reader.accounts.insert(
        address,
        AccountSharedData::new(1, 0, &bpf_loader_upgradeable::id()),
    );
    assert!(
        matches!(SourceBuffer::new_checked(&reader, &address), Err(Error::InvalidBufferAccount(k)) if k == address)
    );
    let mut malformed = buffer(None, &[1]);
    malformed.data_as_mut_slice()[..4].fill(255);
    reader.accounts.insert(address, malformed);
    assert!(matches!(
        SourceBuffer::new_checked(&reader, &address),
        Err(Error::BincodeError(_))
    ));
    reader.accounts.insert(address, buffer(None, &[4, 5, 0, 0]));
    let actual = solana_sha256_hasher::hash(&[4, 5]);
    let source =
        SourceBuffer::new_checked_with_verified_build_hash(&reader, &address, actual).unwrap();
    assert_eq!(source.buffer_address, address);
    assert_eq!(source.buffer_account, reader.accounts[&address]);
    let expected = solana_sha256_hasher::hash(&[8]);
    // Preserve the native variant's actual/expected argument order, including
    // its mismatch with the error display labels.
    assert!(matches!(
        SourceBuffer::new_checked_with_verified_build_hash(&reader, &address, expected),
        Err(Error::BuildHashMismatch(a, e)) if a == actual && e == expected
    ));
}

#[test]
fn builtin_and_v2_prefunding_and_defaults() {
    let address = Pubkey::new_unique();
    let data_address = get_program_data_address(&address);
    let mut reader = Reader::default();
    let target = TargetBuiltin::new_checked(
        &reader,
        &address,
        &CoreBpfMigrationTargetType::Stateless,
        false,
    )
    .unwrap();
    assert_eq!(target.program_address, address);
    assert_eq!(target.program_account, AccountSharedData::default());
    assert_eq!(target.program_data_address, data_address);
    assert_eq!(target.program_data_account_lamports, 0);
    assert_eq!(*reader.reads.borrow(), [address, data_address]);

    for owner in [native_loader::id(), bpf_loader::id()] {
        let mut program = AccountSharedData::new(19, 3, &owner);
        program.set_executable(owner == bpf_loader::id());
        reader.accounts.insert(address, program.clone());
        for prefunded_owner in [system_program::id(), bpf_loader_upgradeable::id()] {
            for lamports in [0, 42] {
                // A present zero-lamport account is not implicitly missing;
                // prefunding checks ownership, not data length or executable.
                let mut prefunded = AccountSharedData::new(lamports, 7, &prefunded_owner);
                prefunded.set_executable(true);
                reader.accounts.insert(data_address, prefunded);
                for allow in [false, true] {
                    reader.reads.borrow_mut().clear();
                    let result = if owner == native_loader::id() {
                        TargetBuiltin::new_checked(
                            &reader,
                            &address,
                            &CoreBpfMigrationTargetType::Builtin,
                            allow,
                        )
                        .map(|t| {
                            (
                                t.program_address,
                                t.program_account,
                                t.program_data_address,
                                t.program_data_account_lamports,
                            )
                        })
                    } else {
                        TargetBpfV2::new_checked(&reader, &address, allow).map(|t| {
                            (
                                t.program_address,
                                t.program_account,
                                t.program_data_address,
                                t.program_data_account_lamports,
                            )
                        })
                    };
                    assert_eq!(*reader.reads.borrow(), [address, data_address]);
                    if allow && prefunded_owner == system_program::id() {
                        assert_eq!(
                            result.unwrap(),
                            (address, program.clone(), data_address, lamports)
                        );
                    } else {
                        assert!(
                            matches!(result, Err(Error::ProgramHasDataAccount(k)) if k == address)
                        );
                    }
                }
            }
        }
    }
    assert!(
        matches!(TargetBuiltin::new_checked(&reader, &address, &CoreBpfMigrationTargetType::Stateless, true), Err(Error::AccountExists(k)) if k == address)
    );
}

#[test]
fn target_checks_stop_before_data_lookup() {
    let address = Pubkey::new_unique();
    let mut reader = Reader::default();
    for owner in [
        None,
        Some(system_program::id()),
        Some(bpf_loader::id()),
        Some(bpf_loader_upgradeable::id()),
    ] {
        if let Some(owner) = owner {
            reader
                .accounts
                .insert(address, AccountSharedData::new(1, 0, &owner));
        }
        reader.reads.borrow_mut().clear();
        let v2 = TargetBpfV2::new_checked(&reader, &address, true).unwrap_err();
        let core = TargetCoreBpf::new_checked(&reader, &address).unwrap_err();
        match owner {
            None => {
                assert!(matches!(v2, Error::AccountNotFound(k) if k == address));
                assert!(matches!(core, Error::AccountNotFound(k) if k == address));
            }
            Some(owner) => {
                if owner == bpf_loader::id() {
                    assert!(matches!(v2, Error::ProgramAccountNotExecutable(k) if k == address));
                } else {
                    assert!(matches!(v2, Error::IncorrectOwner(k) if k == address));
                }
                if owner == bpf_loader_upgradeable::id() {
                    assert!(matches!(core, Error::ProgramAccountNotExecutable(k) if k == address));
                } else {
                    assert!(matches!(core, Error::IncorrectOwner(k) if k == address));
                }
            }
        }
        assert_eq!(*reader.reads.borrow(), [address, address]);
    }
}

#[test]
fn core_bpf_pointer_and_data_checks() {
    let address = Pubkey::new_unique();
    let data_address = get_program_data_address(&address);
    let mut reader = Reader::default();
    reader.accounts.insert(
        address,
        new_target_program_account(|_| 13, &Pubkey::new_unique()).unwrap(),
    );
    assert!(
        matches!(TargetCoreBpf::new_checked(&reader, &address), Err(Error::InvalidProgramAccount(k)) if k == address)
    );
    assert_eq!(*reader.reads.borrow(), [address]);
    reader.accounts.insert(
        address,
        new_target_program_account(|_| 13, &data_address).unwrap(),
    );
    assert!(
        matches!(TargetCoreBpf::new_checked(&reader, &address), Err(Error::ProgramHasNoDataAccount(k)) if k == address)
    );
    reader
        .accounts
        .insert(data_address, AccountSharedData::default());
    assert!(
        matches!(TargetCoreBpf::new_checked(&reader, &address), Err(Error::IncorrectOwner(k)) if k == data_address)
    );
    reader.accounts.insert(data_address, buffer(None, &[]));
    assert!(
        matches!(TargetCoreBpf::new_checked(&reader, &address), Err(Error::InvalidProgramDataAccount(k)) if k == data_address)
    );
    let source = SourceBuffer {
        buffer_address: Pubkey::new_unique(),
        buffer_account: buffer(None, &[9]),
    };
    let data = new_target_program_data_account(|_| 17, 345, &source, None).unwrap();
    reader.accounts.insert(data_address, data.clone());
    let target = TargetCoreBpf::new_checked(&reader, &address).unwrap();
    assert_eq!(target.program_address, address);
    assert_eq!(target.program_data_address, data_address);
    assert_eq!(target.program_data_account, data);
    assert_eq!(target.upgrade_authority_address, None);
}

#[test]
fn synthesis_preserves_layout_authority_and_rent_callback_order() {
    let address = Pubkey::new_unique();
    let authority = Pubkey::new_unique();
    let other = Pubkey::new_unique();
    let elf = [9, 8, 0, 0];
    let source = SourceBuffer {
        buffer_address: address,
        buffer_account: buffer(Some(authority), &elf),
    };
    assert!(
        matches!(new_target_program_data_account(|_| panic!("rent called before authority validation"), 123, &source, Some(other)), Err(Error::UpgradeAuthorityMismatch(a, b)) if a == other && b == Some(authority))
    );
    for provided in [None, Some(authority)] {
        for rent in [Rent::default(), Rent::free()] {
            let calls = RefCell::new(Vec::new());
            let account = new_target_program_data_account(
                |space| {
                    calls.borrow_mut().push(space);
                    rent.minimum_balance(space).max(1)
                },
                123,
                &source,
                provided,
            )
            .unwrap();
            let offset = UpgradeableLoaderState::size_of_programdata_metadata();
            let state = UpgradeableLoaderState::ProgramData {
                slot: 123,
                upgrade_authority_address: provided,
            };
            let mut expected = bincode::serialize(&state).unwrap();
            expected.resize(offset, 0);
            expected.extend_from_slice(&elf);
            assert_eq!(account.data(), expected);
            assert_eq!(*calls.borrow(), [offset + elf.len()]);
            assert_eq!(
                account.lamports(),
                rent.minimum_balance(expected.len()).max(1)
            );
            assert_eq!(account.owner(), &bpf_loader_upgradeable::id());
            assert!(!account.executable());
            assert_eq!(account.rent_epoch(), 0);
        }
    }
    let account = new_target_program_account(
        |space| {
            assert_eq!(space, UpgradeableLoaderState::size_of_program());
            0
        },
        &address,
    )
    .unwrap();
    assert_eq!(account.lamports(), 0); // No hidden rent policy in synthesis.
    assert!(account.executable());
    assert_eq!(account.owner(), &bpf_loader_upgradeable::id());
    assert_eq!(account.rent_epoch(), 0);
    assert_eq!(
        account.data(),
        bincode::serialize(&UpgradeableLoaderState::Program {
            programdata_address: address
        })
        .unwrap()
    );
}

#[test]
fn closure_reader_does_not_convert_storage_panics_to_missing_accounts() {
    let address = Pubkey::new_unique();
    let reader = |_: &Pubkey| -> Option<AccountSharedData> { panic!("fatal backing-store error") };
    assert!(std::panic::catch_unwind(|| SourceBuffer::new_checked(&reader, &address)).is_err());
    let reader = |_: &Pubkey| None;
    assert!(
        matches!(SourceBuffer::new_checked(&reader, &address), Err(Error::AccountNotFound(k)) if k == address)
    );
}

#[test]
fn checked_add_exports_native_overflow_error_for_balances_and_sizes() {
    assert_eq!(checked_add(5u64, 7).unwrap(), 12);
    assert_eq!(checked_add(usize::MAX, 0).unwrap(), usize::MAX);
    assert!(matches!(
        checked_add(u64::MAX, 1),
        Err(Error::ArithmeticOverflow)
    ));
    assert!(matches!(
        checked_add(usize::MAX, 1),
        Err(Error::ArithmeticOverflow)
    ));
}
