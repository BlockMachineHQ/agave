#![cfg(feature = "agave-unstable-api")]

use {
    agave_feature_set::FeatureSet,
    solana_cost_model::cost_tracker::CostTrackerLimits,
    solana_epoch_schedule::EpochSchedule,
    solana_pubkey::Pubkey,
    solana_runtime::{
        slot_params::{
            slot_time_feature_gates, slot_time_feature_ids, SlotParams, SlotParamsArchive,
            DEFAULT_MAX_ENTRY_BYTES_PER_SLOT, LEGACY_HASHES_PER_TICK, LEGACY_SLOT_PARAMS,
        },
        stake_weighted_timestamp::{
            calculate_stake_weighted_timestamp, MaxAllowableDrift,
            MAX_ALLOWABLE_DRIFT_PERCENTAGE_FAST, MAX_ALLOWABLE_DRIFT_PERCENTAGE_SLOW_V2,
        },
    },
    std::{collections::HashMap, time::Duration},
};

#[test]
fn timestamp_median_and_zero_stake() {
    let a = Pubkey::new_unique();
    let b = Pubkey::new_unique();
    let unknown = Pubkey::new_unique();
    let stakes = HashMap::from([(a, (10, ())), (b, (10, ()))]);
    let timestamps = HashMap::from([(a, (5, 100)), (b, (5, 200)), (unknown, (5, i64::MAX))]);
    let drift = MaxAllowableDrift {
        fast: MAX_ALLOWABLE_DRIFT_PERCENTAGE_FAST,
        slow: MAX_ALLOWABLE_DRIFT_PERCENTAGE_SLOW_V2,
    };
    // Exactly half the stake is not enough: the upper median wins.
    assert_eq!(
        calculate_stake_weighted_timestamp(
            &timestamps,
            &stakes,
            5,
            |_, _| Duration::ZERO,
            None,
            drift,
        ),
        Some(200)
    );
    for stakes in [HashMap::new(), HashMap::from([(a, (0, ()))])] {
        assert_eq!(
            calculate_stake_weighted_timestamp(
                timestamps.clone(),
                &stakes,
                5,
                |_, _| Duration::ZERO,
                None,
                drift,
            ),
            None
        );
    }
}

#[test]
fn timestamp_uses_archive_elapsed_duration_and_drift_bounds() {
    let schedule = EpochSchedule::custom(100, 100, false);
    let mut features = FeatureSet::default();
    features.activate(&slot_time_feature_ids()[0], 0);
    let archive = SlotParamsArchive::new(&features, &schedule, LEGACY_SLOT_PARAMS);
    let elapsed = |from, to| {
        if from >= to {
            Duration::ZERO
        } else {
            Duration::from_nanos_u128(archive.slot_range_duration_nanos(from + 1, to))
        }
    };
    // (89, 119] contains ten legacy and twenty 350ms slots: exactly 11 seconds.
    assert_eq!(elapsed(89, 119), Duration::from_secs(11));
    let voter = Pubkey::new_unique();
    let stakes = HashMap::from([(voter, (1, ()))]);
    let drift = MaxAllowableDrift {
        fast: MAX_ALLOWABLE_DRIFT_PERCENTAGE_FAST,
        slow: MAX_ALLOWABLE_DRIFT_PERCENTAGE_SLOW_V2,
    };
    assert_eq!((drift.fast, drift.slow), (25, 150));
    for (timestamp, anchor, expected) in [
        (100, None, 111),
        (0, Some((89, 100)), 109),
        (1_000, Some((89, 100)), 127),
        (i64::MAX, None, i64::MAX),
    ] {
        assert_eq!(
            calculate_stake_weighted_timestamp(
                [(voter, (89, timestamp))],
                &stakes,
                119,
                elapsed,
                anchor,
                drift,
            ),
            Some(expected)
        );
    }
}

#[test]
fn archive_epoch_boundaries_and_parameter_accessors() {
    let schedule = EpochSchedule::custom(100, 100, false);
    let mut features = FeatureSet::default();
    let gates = slot_time_feature_gates();
    assert_eq!(slot_time_feature_ids(), gates.map(|(id, _)| id));
    for (index, (id, _)) in gates.iter().enumerate() {
        features.activate(id, index as u64 * 100 + 50);
    }
    let archive = SlotParamsArchive::new(&features, &schedule, LEGACY_SLOT_PARAMS);
    assert_eq!(archive.baseline_params(), LEGACY_SLOT_PARAMS);
    assert_eq!(archive.param_transitions().count(), 5);
    assert_eq!(archive.params_at_slot(99), LEGACY_SLOT_PARAMS);
    assert_eq!(archive.slot_range_duration_nanos(99, 100), 750_000_000);
    assert_eq!(archive.slot_range_duration_nanos(100, 100), 350_000_000);
    assert_eq!(archive.slot_range_duration_nanos(101, 100), 0);
    assert_eq!(
        LEGACY_SLOT_PARAMS.hashes_per_tick(),
        Some(LEGACY_HASHES_PER_TICK)
    );
    assert_eq!(
        LEGACY_SLOT_PARAMS.max_entry_bytes_per_slot(),
        DEFAULT_MAX_ENTRY_BYTES_PER_SLOT
    );
    for (index, (ns, years, hashes, shreds, bytes, stores, burn, account, block, data)) in [
        (
            350_000_000,
            90_162_645.696,
            54_687,
            28_672,
            18_350_080,
            3_584,
            1_400_000_000,
            21_000_000,
            52_500_000,
            87_500_000,
        ),
        (
            300_000_000,
            105_189_753.312,
            46_875,
            24_576,
            15_728_640,
            3_072,
            1_200_000_000,
            18_000_000,
            45_000_000,
            75_000_000,
        ),
        (
            250_000_000,
            126_227_703.974,
            39_062,
            20_480,
            13_107_200,
            2_560,
            1_000_000_000,
            15_000_000,
            37_500_000,
            62_500_000,
        ),
        (
            200_000_000,
            157_784_629.968,
            31_250,
            16_384,
            10_485_760,
            2_048,
            800_000_000,
            12_000_000,
            30_000_000,
            50_000_000,
        ),
    ]
    .into_iter()
    .enumerate()
    {
        let effective = (index as u64 + 1) * 100;
        assert_eq!(
            SlotParamsArchive::feature_effective_slot(&schedule, effective - 50),
            effective
        );
        let params = archive.params_at_slot(effective);
        assert_eq!(params, gates[index].1);
        assert_eq!(params.ns_per_slot(), ns);
        assert_eq!(params.slots_per_year(), years);
        assert_eq!(params.hashes_per_tick(), Some(hashes));
        assert_eq!(params.max_data_shreds_per_slot(), shreds);
        assert_eq!(params.max_code_shreds_per_slot(), shreds);
        assert_eq!(params.max_entry_bytes_per_slot(), bytes);
        assert_eq!(
            params.partitioned_epoch_rewards_stake_account_stores_per_block(),
            stores
        );
        assert_eq!(params.vat_to_burn_per_epoch(), burn);
        assert_eq!(
            params.cost_limits(false),
            CostTrackerLimits::new(account, block, data)
        );
        assert_eq!(
            params.cost_limits(true),
            CostTrackerLimits::new(account * 100 / 60, block * 100 / 60, data)
        );
    }
}

#[test]
fn archive_custom_baseline_and_out_of_order_features() {
    let schedule = EpochSchedule::custom(100, 100, false);
    let restored = SlotParams::genesis_baseline(300_000_000, 123.0, None, 17);
    let ids = slot_time_feature_ids();
    let mut features = FeatureSet::default();
    features.activate(&ids[0], 0);
    // A longer regime is ignored for this custom baseline.
    assert!(!SlotParamsArchive::any_slot_time_reduction_effective(
        &schedule,
        100,
        &features,
        restored.ns_per_slot()
    ));
    let archive = SlotParamsArchive::new(&features, &schedule, restored);
    assert_eq!(
        archive.param_transitions().collect::<Vec<_>>(),
        vec![(0, restored)]
    );
    assert_eq!(archive.baseline_params().slots_per_year(), 123.0);
    assert_eq!(archive.baseline_params().hashes_per_tick(), None);
    assert_eq!(
        archive
            .baseline_params()
            .partitioned_epoch_rewards_stake_account_stores_per_block(),
        17
    );
    features.activate(&ids[3], 150);
    features.activate(&ids[2], 250);
    features.activate(&ids[1], 150);
    assert!(!SlotParamsArchive::any_slot_time_reduction_effective(
        &schedule,
        199,
        &features,
        restored.ns_per_slot()
    ));
    assert!(SlotParamsArchive::any_slot_time_reduction_effective(
        &schedule,
        200,
        &features,
        restored.ns_per_slot()
    ));
    let archive = SlotParamsArchive::new(&features, &schedule, restored);
    // Same-boundary and later longer regimes cannot increase the duration.
    assert_eq!(
        archive
            .param_transitions()
            .map(|(slot, params)| (slot, params.ns_per_slot()))
            .collect::<Vec<_>>(),
        vec![(0, 300_000_000), (200, 200_000_000)]
    );
    assert_eq!(archive.params_at_slot(u64::MAX).ns_per_slot(), 200_000_000);
    assert_eq!(
        SlotParamsArchive::default().baseline_params(),
        LEGACY_SLOT_PARAMS
    );
}
