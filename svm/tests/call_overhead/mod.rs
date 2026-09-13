//! Manual, width-one native SVM experiment; no scheduler or account store.
//! Run in release mode with --ignored --nocapture --test-threads=1.
use {
    super::*,
    solana_svm_callback::{AccountState, InvokeContextCallback, TransactionProcessingCallback},
    solana_svm_timings::ExecuteTimingType,
    std::{cell::Cell, hint::black_box, time::Instant},
};

const N: usize = 256;
const REPEATS: usize = 64;
const ROUNDS: usize = 12;

// The native fixture's account lookup, with optional diagnostics. Avoid retaining
// inspection histories in timing runs. No account writes occur in this callback:
// all transactions have disjoint writable accounts and each pass starts from the
// identical backing image, as is appropriate for a load/execute-only experiment.
struct Callback<'a> {
    bank: &'a MockBankCallback,
    diagnose: bool,
    reads: Cell<usize>,
}

impl InvokeContextCallback for Callback<'_> {}
impl TransactionProcessingCallback for Callback<'_> {
    fn get_account_shared_data(&self, key: &Pubkey) -> Option<(AccountSharedData, Slot)> {
        if self.diagnose {
            self.reads.set(self.reads.get() + 1);
        }
        self.bank.get_account_shared_data(key)
    }

    fn inspect_account(&self, key: &Pubkey, state: AccountState, writable: bool) {
        if self.diagnose {
            self.bank.inspect_account(key, state, writable);
        }
    }
}

fn fixture(sbf: bool) -> SvmTestEnvironment<'static> {
    let mut entry = SvmTestEntry::default();
    let program = program_address("write-to-account");
    if sbf {
        entry.add_initial_program("write-to-account");
    }
    for _ in 0..N {
        let payer = Keypair::new();
        let target = Pubkey::new_unique();
        entry.add_initial_account(
            payer.pubkey(),
            &AccountSharedData::new(10 * LAMPORTS_PER_SOL, 0, &system_program::id()),
        );
        entry.add_initial_account(
            target,
            &AccountSharedData::new(
                LAMPORTS_PER_SOL,
                usize::from(sbf),
                &if sbf { program } else { system_program::id() },
            ),
        );
        let instruction = if sbf {
            Instruction::new_with_bytes(program, &[1], vec![AccountMeta::new(target, false)])
        } else {
            system_instruction::transfer(&payer.pubkey(), &target, 123)
        };
        entry.push_transaction(Transaction::new_signed_with_payer(
            &[instruction],
            Some(&payer.pubkey()),
            &[&payer],
            LAST_BLOCKHASH,
        ));
    }
    let mut env = SvmTestEnvironment::create(entry);
    env.processing_config.recording_config = ExecutionRecordingConfig::default();
    env.processing_config.check_program_deployment_slot = true;
    env
}

fn call(
    env: &SvmTestEnvironment,
    callback: &Callback,
    txs: &[SanitizedTransaction],
    checks: Vec<TransactionCheckResult>,
) -> LoadAndExecuteSanitizedTransactionsOutput {
    env.batch_processor.load_and_execute_sanitized_transactions(
        callback,
        txs,
        checks,
        &env.processing_environment,
        &env.processing_config,
    )
}

fn cache_state(env: &SvmTestEnvironment) -> Vec<(Pubkey, Arc<ProgramCacheEntry>, u64, u64)> {
    let mut entries: Vec<_> = env
        .batch_processor
        .global_program_cache
        .read()
        .unwrap()
        .get_flattened_entries_for_tests()
        .into_iter()
        .map(|(key, entry)| {
            let uses = entry.stats.uses.load(Ordering::Relaxed);
            let compilations = entry.stats.compilations.load(Ordering::Relaxed);
            (key, entry, uses, compilations)
        })
        .collect();
    entries.sort_by_key(|entry| entry.0);
    entries
}

fn diagnose(
    env: &SvmTestEnvironment,
    txs: &[SanitizedTransaction],
    checks: &[TransactionCheckResult],
    batch: usize,
) -> Vec<TransactionProcessingResult> {
    let before = cache_state(env);
    let hits_before = env
        .batch_processor
        .global_program_cache
        .read()
        .unwrap()
        .stats
        .hits
        .load(Ordering::Relaxed);
    let callback = Callback {
        bank: &env.mock_bank,
        diagnose: true,
        reads: Cell::new(0),
    };
    let mut results = Vec::with_capacity(N);
    for (txs, checks) in txs.chunks(batch).zip(checks.chunks(batch)) {
        let output = call(env, &callback, txs, checks.to_vec());
        assert!(output.balance_collector.is_none());
        assert_eq!(
            output.execute_timings.details.create_executor_load_elf_us.0,
            0
        );
        assert_eq!(
            output
                .execute_timings
                .details
                .create_executor_verify_code_us
                .0,
            0
        );
        assert_eq!(
            output
                .execute_timings
                .details
                .create_executor_jit_compile_us
                .0,
            0
        );
        results.extend(output.processing_results);
    }
    let after = cache_state(env);
    let hits_after = env
        .batch_processor
        .global_program_cache
        .read()
        .unwrap()
        .stats
        .hits
        .load(Ordering::Relaxed);
    assert_eq!(before.len(), after.len());
    let mut uses_delta = 0;
    for (before, after) in before.iter().zip(&after) {
        assert_eq!(before.0, after.0);
        assert!(Arc::ptr_eq(&before.1, &after.1));
        assert_eq!(before.3, after.3);
        uses_delta += after.2 - before.2;
    }
    assert_eq!(uses_delta, N as u64);
    println!(
        "diagnostic batch={batch} backing_reads={} cache_uses_delta={uses_delta} cache_hits_delta={}",
        callback.reads.get(),
        hits_after - hits_before
    );
    results
}

fn assert_outputs_equal(
    reference: &[TransactionProcessingResult],
    actual: &[TransactionProcessingResult],
) {
    assert_eq!(reference.len(), N);
    assert_eq!(actual.len(), N);
    for (reference, actual) in reference.iter().zip(actual) {
        let (
            Ok(ProcessedTransaction::Executed(reference)),
            Ok(ProcessedTransaction::Executed(actual)),
        ) = (reference, actual)
        else {
            panic!("expected successful execution: {reference:?} {actual:?}");
        };
        assert!(reference.was_successful());
        assert!(actual.was_successful());
        assert_eq!(reference.loaded_transaction, actual.loaded_transaction);
        assert_eq!(reference.execution_details, actual.execution_details);
        assert!(reference.programs_modified_by_tx.is_empty());
        assert!(actual.programs_modified_by_tx.is_empty());
    }
}

// Check-vector preparation, output destruction and metric aggregation are
// outside each timed call. Returned metrics use integer microseconds and nest;
// they cannot be subtracted from wall time to infer an exact setup residual.
fn sample(
    env: &SvmTestEnvironment,
    txs: &[SanitizedTransaction],
    checks: &[TransactionCheckResult],
    batch: usize,
) -> (f64, [f64; 4]) {
    let callback = Callback {
        bank: &env.mock_bank,
        diagnose: false,
        reads: Cell::new(0),
    };
    let mut elapsed = 0;
    let mut native = [0_u64; 4];
    for _ in 0..REPEATS {
        for (txs, checks) in txs.chunks(batch).zip(checks.chunks(batch)) {
            let checks = checks.to_vec();
            let start = Instant::now();
            let output = black_box(call(env, &callback, black_box(txs), checks));
            elapsed += start.elapsed().as_nanos();
            for (total, metric) in native.iter_mut().zip([
                ExecuteTimingType::ValidateFeesUs,
                ExecuteTimingType::LoadUs,
                ExecuteTimingType::ExecuteUs,
                ExecuteTimingType::ProgramCacheUs,
            ]) {
                *total += output.execute_timings.metrics[metric].0;
            }
            black_box(output);
        }
    }
    let count = (N * REPEATS) as f64;
    (
        elapsed as f64 / count,
        native.map(|v| v as f64 * 1000.0 / count),
    )
}

#[test]
#[ignore = "manual warmed native SVM benchmark; use release and --nocapture --test-threads=1"]
#[allow(clippy::assertions_on_constants)] // Reject debug runs, but allow debug compilation.
fn bench_warmed_singleton_vs_small_batches() {
    assert!(!cfg!(debug_assertions), "timings require --release");
    println!(
        "width=1 n={N} repeats={REPEATS} rounds={ROUNDS} arch={}",
        std::env::consts::ARCH
    );
    for sbf in [false, true] {
        let env = fixture(sbf);
        let (txs, checks) = env.test_entry.prepare_transactions();
        let callback = Callback {
            bank: &env.mock_bank,
            diagnose: false,
            reads: Cell::new(0),
        };
        let image = env.mock_bank.account_shared_data.read().unwrap().clone();
        // Warm every transaction and program, without committing account writes.
        for _ in 0..4 {
            let output = call(&env, &callback, &txs, checks.clone());
            assert!(
                output
                    .processing_results
                    .iter()
                    .all(|r| r.was_processed_with_successful_result())
            );
        }
        println!(
            "workload={}",
            if sbf { "sbf-write" } else { "builtin-transfer" }
        );
        let reference = diagnose(&env, &txs, &checks, 1);
        let inspections = std::mem::take(&mut *env.mock_bank.inspected_accounts.write().unwrap());
        // Independent expected effects, not merely agreement between two arms.
        for (tx, result) in txs.iter().zip(&reference) {
            let executed = result.as_ref().unwrap().executed_transaction().unwrap();
            let payer = &executed.loaded_transaction.accounts[0].1;
            assert_eq!(
                payer.lamports(),
                10 * LAMPORTS_PER_SOL - LAMPORTS_PER_SIGNATURE - if sbf { 0 } else { 123 }
            );
            let target = *tx
                .account_keys()
                .iter()
                .enumerate()
                .find(|(index, key)| **key != *tx.fee_payer() && tx.is_writable(*index))
                .unwrap()
                .1;
            let target = &executed
                .loaded_transaction
                .accounts
                .iter()
                .find(|(key, _)| *key == target)
                .unwrap()
                .1;
            if sbf {
                assert_eq!(target.data(), &[100]);
            } else {
                assert_eq!(target.lamports(), LAMPORTS_PER_SOL + 123);
            }
        }
        for batch in [2, 4, 8, 16] {
            let results = diagnose(&env, &txs, &checks, batch);
            assert_outputs_equal(&reference, &results);
            assert_eq!(
                inspections,
                std::mem::take(&mut *env.mock_bank.inspected_accounts.write().unwrap())
            );
        }
        assert_eq!(image, *env.mock_bank.account_shared_data.read().unwrap());
        println!(
            "equivalence=exact_loaded_transactions_execution_details_inspections; backing_image=unchanged"
        );
        // Reverse the entire order on odd rounds, pairing each batch with a
        // fresh singleton control and alternating AB/BA to expose drift.
        for round in 0..ROUNDS {
            let batches = if round % 2 == 0 {
                [2, 4, 8, 16]
            } else {
                [16, 8, 4, 2]
            };
            for batch in batches {
                let order = if round % 2 == 0 {
                    [1, batch]
                } else {
                    [batch, 1]
                };
                for size in order {
                    let (ns, native) = sample(&env, &txs, &checks, size);
                    println!(
                        "sample sbf={sbf} round={round} pair={batch} batch={size} ns_per_tx={ns:.3} validate_ns={:.3} load_ns={:.3} execute_ns={:.3} cache_ns={:.3}",
                        native[0], native[1], native[2], native[3]
                    );
                }
            }
        }
        // Empty calls exercise actual native setup/teardown, including the
        // builtin-cache clone, but are not a separable attribution of its cost.
        for round in 0..ROUNDS {
            let start = Instant::now();
            for _ in 0..N * REPEATS {
                black_box(call(&env, &callback, &[], Vec::new()));
            }
            println!(
                "empty sbf={sbf} round={round} ns_per_call={:.3}",
                start.elapsed().as_nanos() as f64 / (N * REPEATS) as f64
            );
        }
        // Isolated native type clone + uncontended read lock + drop. Reuse the
        // five actual registered builtin entries from this fixture. This is a
        // component probe, not a subtraction-based attribution of full calls.
        let mut builtins =
            solana_program_runtime::loaded_programs::ProgramCacheForTxBatch::new(EXECUTION_SLOT);
        for (key, entry, _, _) in cache_state(&env) {
            if key != program_address("write-to-account") {
                builtins.replenish(key, entry);
            }
        }
        let builtins = RwLock::new(builtins);
        for round in 0..ROUNDS {
            let start = Instant::now();
            for _ in 0..N * REPEATS {
                black_box(black_box(&builtins).read().unwrap().clone());
            }
            println!(
                "builtin_clone sbf={sbf} round={round} ns_per_call={:.3}",
                start.elapsed().as_nanos() as f64 / (N * REPEATS) as f64
            );
        }
    }
}
