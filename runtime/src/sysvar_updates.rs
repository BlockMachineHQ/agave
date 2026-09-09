//! Bank-free sysvar account fields, rent funding, and ordered epoch rent updates.

use {
    agave_feature_set as feature_set,
    solana_account::{
        AccountSharedData, InheritableAccountFields, ReadableAccount, WritableAccount,
    },
    solana_clock::INITIAL_RENT_EPOCH,
    solana_pubkey::Pubkey,
    solana_rent::Rent,
};

/// Preserve only lamports and rent epoch when reconstructing an account.
pub fn inherit_specially_retained_account_fields(
    old_account: Option<&AccountSharedData>,
) -> InheritableAccountFields {
    const RENT_UNADJUSTED_INITIAL_BALANCE: u64 = 1;
    (
        old_account
            .map(|a| a.lamports())
            .unwrap_or(RENT_UNADJUSTED_INITIAL_BALANCE),
        old_account
            .map(|a| a.rent_epoch())
            .unwrap_or(INITIAL_RENT_EPOCH),
    )
}

/// Fund the current data length without reducing an existing balance or changing other fields.
pub fn adjust_sysvar_balance_for_rent(rent: &Rent, account: &mut AccountSharedData) {
    account.set_lamports(
        rent.minimum_balance(account.data().len())
            .max(1)
            .max(account.lamports()),
    );
}

/// Apply SIMD-0194's exact floating-point conversion and legacy burn percentage.
#[allow(deprecated)]
pub fn deprecate_rent_exemption_threshold(rent: &Rent) -> Rent {
    Rent {
        lamports_per_byte: (rent.lamports_per_byte as f64
            * f64::from_le_bytes(rent.exemption_threshold)) as u64,
        exemption_threshold: 1.0f64.to_le_bytes(),
        burn_percent: 50,
    }
}

/// Plan each rent update from the actual current rent and selected feature membership.
///
/// At an epoch boundary, select newly activated features, not all active features.
/// Callers must install and fund/store the sysvar for EACH returned value, in order;
/// applying only the final value can lose intermediate rent funding. An empty selection
/// returns an empty plan. This does not perform genesis or snapshot initialization.
pub fn rent_activation_updates(
    current_rent: &Rent,
    is_selected: impl Fn(&Pubkey) -> bool,
) -> Vec<Rent> {
    let mut rent = current_rent.clone();
    let mut updates = Vec::new();
    if is_selected(&feature_set::deprecate_rent_exemption_threshold::id()) {
        rent = deprecate_rent_exemption_threshold(&rent);
        updates.push(rent.clone());
    }

    // SIMD-0437 assumes SIMD-0194 has deprecated the threshold. Multiple gates
    // use the lowest selected value; later epochs may select a higher value.
    let rent_feature_gates = [
        (
            feature_set::set_lamports_per_byte_to_6333::id(),
            feature_set::set_lamports_per_byte_to_6333::LAMPORTS_PER_BYTE,
        ),
        (
            feature_set::set_lamports_per_byte_to_5080::id(),
            feature_set::set_lamports_per_byte_to_5080::LAMPORTS_PER_BYTE,
        ),
        (
            feature_set::set_lamports_per_byte_to_2575::id(),
            feature_set::set_lamports_per_byte_to_2575::LAMPORTS_PER_BYTE,
        ),
        (
            feature_set::set_lamports_per_byte_to_1322::id(),
            feature_set::set_lamports_per_byte_to_1322::LAMPORTS_PER_BYTE,
        ),
        (
            feature_set::set_lamports_per_byte_to_696::id(),
            feature_set::set_lamports_per_byte_to_696::LAMPORTS_PER_BYTE,
        ),
        // SIMD-0438 must override any SIMD-0437 gate selected in the same epoch.
        (
            feature_set::set_lamports_per_byte_to_6960::id(),
            feature_set::set_lamports_per_byte_to_6960::LAMPORTS_PER_BYTE,
        ),
    ];
    for (feature_id, lamports_per_byte) in rent_feature_gates {
        if is_selected(&feature_id) {
            rent.lamports_per_byte = lamports_per_byte;
            updates.push(rent.clone());
        }
    }
    updates
}
