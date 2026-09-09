#![cfg(feature = "agave-unstable-api")]
#![allow(deprecated)]

use {
    agave_feature_set as feature_set,
    solana_account::{
        AccountSharedData, ReadableAccount, WritableAccount, create_account_shared_data_with_fields,
    },
    solana_clock::INITIAL_RENT_EPOCH,
    solana_pubkey::Pubkey,
    solana_rent::Rent,
    solana_runtime::sysvar_updates::{
        adjust_sysvar_balance_for_rent, deprecate_rent_exemption_threshold,
        inherit_specially_retained_account_fields, rent_activation_updates,
    },
};

#[test]
fn inherited_fields_and_sdk_constructor() {
    assert_eq!(
        inherit_specially_retained_account_fields(None),
        (1, INITIAL_RENT_EPOCH)
    );
    for lamports in [0, 1, u64::MAX] {
        for rent_epoch in [INITIAL_RENT_EPOCH, 937, u64::MAX] {
            let mut old = AccountSharedData::new(lamports, 13, &Pubkey::new_unique());
            old.set_rent_epoch(rent_epoch);
            old.set_executable(true);
            let fields = inherit_specially_retained_account_fields(Some(&old));
            assert_eq!(fields, (lamports, rent_epoch));
            let rent = Rent::default();
            let new = create_account_shared_data_with_fields(&rent, fields);
            assert_eq!(new.lamports(), lamports);
            assert_eq!(new.rent_epoch(), rent_epoch);
            assert_eq!(new.owner(), &solana_sdk_ids::sysvar::id());
            assert!(!new.executable());
            assert_eq!(bincode::deserialize::<Rent>(new.data()).unwrap(), rent);
        }
    }
}

#[test]
fn rent_floors_preserve_other_fields_and_overfunding() {
    for rent in [Rent::default(), Rent::free()] {
        for len in [0, 1, 97] {
            let floor = rent.minimum_balance(len).max(1);
            for balance in [0, 1, floor - 1, floor, floor + 123, u64::MAX] {
                for epoch in [0, 937, u64::MAX] {
                    let mut account = AccountSharedData::new(balance, len, &Pubkey::new_unique());
                    account.set_rent_epoch(epoch);
                    account.set_executable(true);
                    let mut expected = account.clone();
                    expected.set_lamports(balance.max(floor));
                    adjust_sysvar_balance_for_rent(&rent, &mut account);
                    assert_eq!(account, expected);
                    adjust_sysvar_balance_for_rent(&rent, &mut account);
                    assert_eq!(account, expected);
                }
            }
        }
    }
}

#[test]
fn deprecation_preserves_native_float_conversion() {
    for (lamports_per_byte, threshold, expected) in [
        (3, 1.5, 4),
        ((1u64 << 53) + 1, 1.0, 1u64 << 53),
        (u64::MAX, 2.0, u64::MAX),
        (7, -1.0, 0),
        (7, f64::NAN, 0),
        (7, f64::INFINITY, u64::MAX),
        (0, 2.0, 0),
    ] {
        let current = Rent {
            lamports_per_byte,
            exemption_threshold: threshold.to_le_bytes(),
            burn_percent: 17,
        };
        let updated = deprecate_rent_exemption_threshold(&current);
        assert_eq!(updated.lamports_per_byte, expected);
        assert_eq!(updated.exemption_threshold, 1.0f64.to_le_bytes());
        assert_eq!(updated.burn_percent, 50);
        assert_eq!(
            rent_activation_updates(&current, |id| {
                *id == feature_set::deprecate_rent_exemption_threshold::id()
            }),
            vec![updated]
        );
        assert_eq!(current.burn_percent, 17);
    }
}

#[test]
fn all_128_gate_subsets_have_native_order_and_intermediate_values() {
    let gates = [
        feature_set::deprecate_rent_exemption_threshold::id(),
        feature_set::set_lamports_per_byte_to_6333::id(),
        feature_set::set_lamports_per_byte_to_5080::id(),
        feature_set::set_lamports_per_byte_to_2575::id(),
        feature_set::set_lamports_per_byte_to_1322::id(),
        feature_set::set_lamports_per_byte_to_696::id(),
        feature_set::set_lamports_per_byte_to_6960::id(),
    ];
    // Deliberately not defaults: detect guessed starting state or implicit deprecation.
    let current = Rent {
        lamports_per_byte: 9999,
        exemption_threshold: 1.5f64.to_le_bytes(),
        burn_percent: 17,
    };
    for mask in 0u8..128 {
        let mut selected: Vec<_> = gates
            .iter()
            .enumerate()
            .filter(|(index, _)| mask & (1 << index) != 0)
            .map(|(_, id)| *id)
            .collect();
        let mut expected = Vec::new();
        let mut rent = current.clone();
        for (index, value) in [14998, 6333, 5080, 2575, 1322, 696, 6960]
            .into_iter()
            .enumerate()
        {
            if mask & (1 << index) != 0 {
                rent.lamports_per_byte = value;
                if index == 0 {
                    rent.exemption_threshold = 1.0f64.to_le_bytes();
                    rent.burn_percent = 50;
                }
                expected.push(rent.clone());
            }
        }
        let plan = rent_activation_updates(&current, |id| selected.contains(id));
        assert_eq!(plan, expected, "mask {mask}");
        selected.reverse();
        if !selected.is_empty() {
            selected.rotate_left(1);
        }
        selected.push(Pubkey::new_unique());
        assert_eq!(
            rent_activation_updates(&current, |id| selected.contains(id)),
            plan
        );

        // The same explicit plan can be preflighted and then staged without recomputing gates.
        let mut preflight = AccountSharedData::new(1, 17, &solana_sdk_ids::sysvar::id());
        preflight.set_rent_epoch(937);
        let mut staged = preflight.clone();
        for rent in &plan {
            adjust_sysvar_balance_for_rent(rent, &mut preflight);
        }
        for rent in &plan {
            adjust_sysvar_balance_for_rent(rent, &mut staged);
        }
        assert_eq!(preflight, staged);
        assert_eq!(staged.rent_epoch(), 937);
        assert_eq!(
            staged.lamports(),
            expected
                .iter()
                .map(|rent| rent.minimum_balance(17).max(1))
                .max()
                .unwrap_or(1)
        );
    }
}
