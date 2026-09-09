#![cfg(feature = "agave-unstable-api")]

mod rewards {
    use {
        solana_instruction::error::InstructionError,
        solana_pubkey::Pubkey,
        solana_runtime::{
            inflation_rewards::{
                adjust_delegation_for_rent, delegation_may_need_adjustment,
                points::{
                    calculate_points_for_tower, CalculationEnvironment, DelegatedVoteState,
                    InflationPointCalculationEvent, PointValue,
                },
                redeem_rewards_for_tower,
            },
            stake_delegation::{delegation_activation_status, delegation_effective_stake},
        },
        solana_stake_interface::{
            error::StakeError,
            stake_flags::StakeFlags,
            stake_history::{StakeHistory, StakeHistoryEntry},
            state::{Delegation, Meta, Stake, StakeStateV2},
        },
        std::cell::RefCell,
    };

    const NO_TRACER: Option<fn(&InflationPointCalculationEvent)> = None;

    #[test]
    fn distribution_rent_cap_uses_current_balance_and_deactivates_at_zero() {
        let history = StakeHistory::default();
        for fixed in [false, true] {
            let stake = Stake {
                delegation: Delegation::new(&Pubkey::new_unique(), 100, u64::MAX),
                credits_observed: 0,
            };
            let (reward, voter, updated) = redeem_rewards_for_tower(
                stake,
                0,
                vote(5),
                CalculationEnvironment {
                    rewarded_epoch: 1,
                    point_value: &PointValue {
                        rewards: 10,
                        points: 500,
                    },
                    stake_history: &history,
                    new_rate_activation_epoch: None,
                    commission_rate_in_basis_points: true,
                    adjust_delegations_for_rent: true,
                    use_fixed_point_stake_math: fixed,
                },
                NO_TRACER,
                120,
                50,
            )
            .unwrap();
            assert_eq!(
                (
                    reward,
                    voter,
                    updated.delegation.stake,
                    updated.credits_observed
                ),
                (10, 0, 110, 5)
            );
            // Distribution uses current post-reward lamports/rent, not the calculation inputs.
            for (lamports, minimum, expected) in [
                (130, 50, 80),
                (1000, 50, 110), // funded between calculation and distribution
                (130, 100, 30),  // rent increased
                (50, 50, 0),
                (40, 50, 0), // saturating subtraction
            ] {
                let mut distributed = updated;
                adjust_delegation_for_rent(&mut distributed.delegation, 1, 110, lamports, minimum);
                let mut expected_stake = updated;
                expected_stake.delegation.stake = expected;
                if expected == 0 {
                    expected_stake.delegation.deactivation_epoch = 1;
                }
                assert_eq!(distributed, expected_stake);
            }
            // An already-zero delegation does not change its deactivation epoch.
            let mut zero = Delegation {
                stake: 0,
                deactivation_epoch: 7,
                ..stake.delegation
            };
            let before = zero;
            adjust_delegation_for_rent(&mut zero, 1, 0, 40, 50);
            assert_eq!(zero, before);
        }
    }

    #[test]
    fn tower_vote_view_partial_credits_and_migration_marker() {
        use {
            agave_votor_messages::migration::AG_MIGRATION_EPOCH_CREDIT,
            solana_vote::vote_state_view::VoteStateView,
            solana_vote_program::vote_state::{VoteStateV4, VoteStateVersions},
            std::sync::Arc,
        };
        let history = StakeHistory::default();
        let stake = Stake {
            delegation: Delegation::new(&Pubkey::new_unique(), 100, u64::MAX),
            credits_observed: 2,
        };
        for fixed in [false, true] {
            for marker in [false, true] {
                let mut epoch_credits = vec![(0, 3, 0), (1, 5, 3)];
                if marker {
                    epoch_credits.extend([AG_MIGRATION_EPOCH_CREDIT, (2, 10, 5)]);
                }
                let view = VoteStateView::try_new(Arc::new(
                    bincode::serialize(&VoteStateVersions::new_v4(VoteStateV4 {
                        epoch_credits,
                        ..VoteStateV4::default()
                    }))
                    .unwrap(),
                ))
                .unwrap();
                let delegated = DelegatedVoteState::from(&view);
                assert_eq!(delegated.credits, if marker { 10 } else { 5 });
                // Tower's native iterator stops at the migration marker.
                assert_eq!(
                    calculate_points_for_tower(
                        &StakeStateV2::Stake(Meta::default(), stake, StakeFlags::empty()),
                        delegated,
                        &history,
                        None,
                        fixed,
                    ),
                    Ok(300)
                );
                let (staker, voter, updated) = redeem_rewards_for_tower(
                    stake,
                    0,
                    DelegatedVoteState::from(&view),
                    CalculationEnvironment {
                        rewarded_epoch: 1,
                        point_value: &PointValue {
                            rewards: 3,
                            points: 300,
                        },
                        stake_history: &history,
                        new_rate_activation_epoch: None,
                        commission_rate_in_basis_points: true,
                        adjust_delegations_for_rent: false,
                        use_fixed_point_stake_math: fixed,
                    },
                    NO_TRACER,
                    1000,
                    50,
                )
                .unwrap();
                assert_eq!((staker, voter, updated.credits_observed), (3, 0, 5));
                assert_eq!(updated.delegation.stake, 103);
            }
        }
    }

    #[test]
    fn tower_rent_adjustment_saturates_delegation() {
        let history = StakeHistory::default();
        for fixed in [false, true] {
            let stake = Stake {
                delegation: Delegation::new(&Pubkey::new_unique(), u64::MAX, u64::MAX),
                credits_observed: 0,
            };
            let result = redeem_rewards_for_tower(
                stake,
                0,
                vote(1),
                CalculationEnvironment {
                    rewarded_epoch: 1,
                    point_value: &PointValue {
                        rewards: 1,
                        points: u128::from(u64::MAX),
                    },
                    stake_history: &history,
                    new_rate_activation_epoch: None,
                    commission_rate_in_basis_points: false,
                    adjust_delegations_for_rent: true,
                    use_fixed_point_stake_math: fixed,
                },
                NO_TRACER,
                u64::MAX,
                0,
            )
            .unwrap();
            assert_eq!((result.0, result.1), (1, 0));
            assert_eq!(result.2.delegation.stake, u64::MAX);
            assert_eq!(result.2.credits_observed, 1);
        }
    }

    fn vote(credits: u64) -> DelegatedVoteState<'static> {
        DelegatedVoteState {
            credits,
            epoch_credits_iter: Box::new(std::iter::once((1, credits, 0))),
        }
    }

    #[test]
    fn tower_points_redemption_and_feature_dispatch() {
        let stake = Stake {
            delegation: Delegation::new(&Pubkey::new_unique(), 342_898_401_157_885_026, 0),
            credits_observed: 0,
        };
        let state = StakeStateV2::Stake(Meta::default(), stake, StakeFlags::empty());
        let mut history = StakeHistory::default();
        history.add(
            0,
            StakeHistoryEntry {
                effective: 708_104_488_956_562_499,
                activating: 2_426_138_261_763_124_479,
                deactivating: 0,
            },
        );
        for (fixed, expected) in [
            (false, 9_007_199_253_579_466),
            (true, 9_007_199_253_579_461),
        ] {
            assert_eq!(
                delegation_effective_stake(&stake.delegation, 1, &history, Some(0), fixed),
                expected
            );
            let status =
                delegation_activation_status(&stake.delegation, 1, &history, Some(0), fixed);
            assert_eq!(
                status,
                StakeHistoryEntry {
                    effective: expected,
                    activating: stake.delegation.stake - expected,
                    deactivating: 0,
                }
            );
            assert_eq!(
                calculate_points_for_tower(&state, vote(1), &history, Some(0), fixed),
                Ok(u128::from(expected))
            );
            let redeemed = redeem_rewards_for_tower(
                stake,
                0,
                vote(1),
                CalculationEnvironment {
                    rewarded_epoch: 1,
                    point_value: &PointValue {
                        rewards: 1,
                        points: 1,
                    },
                    stake_history: &history,
                    new_rate_activation_epoch: Some(0),
                    commission_rate_in_basis_points: true,
                    adjust_delegations_for_rent: false,
                    use_fixed_point_stake_math: fixed,
                },
                NO_TRACER,
                u64::MAX,
                0,
            )
            .unwrap();
            assert_eq!((redeemed.0, redeemed.1), (expected, 0));
            assert_eq!(redeemed.2.credits_observed, 1);
            assert_eq!(
                redeemed.2.delegation.stake,
                stake.delegation.stake + expected
            );

            // Exercise rate-activation boundaries, missing history and cooldown against the SDK.
            for rate in [None, Some(0), Some(2)] {
                for epoch in 0..4 {
                    let mut delegation = stake.delegation;
                    delegation.deactivation_epoch = 1;
                    #[allow(deprecated)]
                    let expected = if fixed {
                        delegation.stake_activating_and_deactivating_v2(epoch, &history, rate)
                    } else {
                        delegation.stake_activating_and_deactivating(epoch, &history, rate)
                    };
                    assert_eq!(
                        delegation_activation_status(&delegation, epoch, &history, rate, fixed),
                        expected
                    );
                    assert_eq!(
                        delegation_effective_stake(&delegation, epoch, &history, rate, fixed),
                        expected.effective
                    );
                }
            }
        }
    }

    #[test]
    fn tower_failures_rounding_rewind_and_rent_adjustment() {
        let history = StakeHistory::default();
        let base = Stake {
            delegation: Delegation::new(&Pubkey::new_unique(), 100, u64::MAX),
            credits_observed: 0,
        };
        for fixed in [false, true] {
            for state in [
                StakeStateV2::Uninitialized,
                StakeStateV2::Initialized(Meta::default()),
                StakeStateV2::RewardsPool,
            ] {
                assert_eq!(
                    calculate_points_for_tower(&state, vote(5), &history, None, fixed),
                    Err(InstructionError::InvalidAccountData)
                );
            }
            // (credits observed, vote credits, pool rewards, pool points, commission,
            //  rent adjustment, lamports, expected staker/voter/credits)
            for (observed, credits, rewards, points, commission, adjust, lamports, expected) in [
                (0, 5, 1005, 500, 1000, false, 1000, Some((904, 100, 5))),
                (0, 5, 1, 500, 1000, false, 1000, None), // unfair split
                (0, 5, 1, 501, 0, false, 1000, None),    // fractional reward
                (0, 5, 10, 0, 0, false, 1000, None),     // zero denominator
                (5, 5, 10, 500, 0, false, 1000, None),   // no new credits
                (9, 5, 10, 500, 0, false, 1000, Some((0, 0, 5))), // rewind
                (0, 5, 0, 500, 0, false, 1000, Some((0, 0, 5))), // disabled inflation
                (0, 5, 10, 500, 0, true, 120, Some((10, 0, 5))), // uncapped at calculation
                (5, 5, 10, 500, 0, true, 120, Some((0, 0, 5))), // adjustment only
                (5, 5, 10, 500, 0, true, 1000, None),
                (0, 5, 10, 500, 10_000, false, 1000, Some((0, 10, 5))),
                (0, 5, 10, 500, 20_000, false, 1000, Some((0, 10, 5))),
            ] {
                let stake = Stake {
                    credits_observed: observed,
                    ..base
                };
                assert_eq!(
                    calculate_points_for_tower(
                        &StakeStateV2::Stake(Meta::default(), stake, StakeFlags::empty()),
                        vote(credits),
                        &history,
                        None,
                        fixed,
                    ),
                    Ok(u128::from(credits.saturating_sub(observed)) * 100)
                );
                let result = redeem_rewards_for_tower(
                    stake,
                    commission,
                    vote(credits),
                    CalculationEnvironment {
                        rewarded_epoch: 1,
                        point_value: &PointValue { rewards, points },
                        stake_history: &history,
                        new_rate_activation_epoch: None,
                        commission_rate_in_basis_points: true,
                        adjust_delegations_for_rent: adjust,
                        use_fixed_point_stake_math: fixed,
                    },
                    NO_TRACER,
                    lamports,
                    50,
                );
                match expected {
                    Some((staker, voter, new_credits)) => {
                        let (actual_staker, actual_voter, updated) = result.unwrap();
                        assert_eq!(
                            (actual_staker, actual_voter, updated.credits_observed),
                            (staker, voter, new_credits)
                        );
                        assert_eq!(updated.delegation.stake, 100 + staker);
                    }
                    None => assert_eq!(result, Err(StakeError::NoCreditsToRedeem.into())),
                }
            }
            let active = delegation_activation_status(&base.delegation, 1, &history, None, fixed);
            assert!(delegation_may_need_adjustment(100, 100, 120, 50, active));
            assert!(!delegation_may_need_adjustment(
                100,
                100,
                120,
                50,
                StakeHistoryEntry::default()
            ));
        }
    }

    #[test]
    fn tower_activation_and_commission_trace_preserve_explicit_inputs() {
        let history = StakeHistory::default();
        for fixed in [false, true] {
            for bps in [false, true] {
                let traces = RefCell::new(Vec::new());
                let stake = Stake {
                    delegation: Delegation::new(&Pubkey::new_unique(), 100, 1),
                    credits_observed: 0,
                };
                let result = redeem_rewards_for_tower(
                    stake,
                    1200,
                    vote(5),
                    CalculationEnvironment {
                        rewarded_epoch: 1,
                        point_value: &PointValue {
                            rewards: 100,
                            points: 500,
                        },
                        stake_history: &history,
                        new_rate_activation_epoch: None,
                        commission_rate_in_basis_points: bps,
                        adjust_delegations_for_rent: false,
                        use_fixed_point_stake_math: fixed,
                    },
                    Some(|event: &InflationPointCalculationEvent| match event {
                        InflationPointCalculationEvent::Commission(value) => {
                            traces.borrow_mut().push((false, u16::from(*value)))
                        }
                        InflationPointCalculationEvent::CommissionBps(value) => {
                            traces.borrow_mut().push((true, *value))
                        }
                        _ => (),
                    }),
                    1000,
                    50,
                )
                .unwrap();
                assert_eq!((result.0, result.1, result.2.credits_observed), (0, 0, 5));
                assert_eq!(*traces.borrow(), vec![(bps, if bps { 1200 } else { 12 })]);
            }
        }
    }
}

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
