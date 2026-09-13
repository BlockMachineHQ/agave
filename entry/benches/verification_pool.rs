//! Same-input native verification experiment; run with `cargo bench -p
//! solana-entry --bench verification_pool`. No Bank, execution, or I/O is timed.
use {
    rayon::{ThreadPool, ThreadPoolBuilder, prelude::*},
    solana_entry::entry::{
        Entry, EntrySlice, EntryType, ValidatedHashedTransactions, entries_to_verification_data,
        next_versioned_entry, validate_and_hash_transactions, verify_entries_cpu_in_pool,
    },
    solana_hash::Hash,
    solana_keypair::Keypair,
    solana_runtime_transaction::runtime_transaction::RuntimeTransaction,
    solana_signature::Signature,
    solana_transaction::sanitized::SanitizedTransaction,
    solana_transaction_error::TransactionError,
    std::{hint::black_box, time::Instant},
};

mod support;

type Validated = ValidatedHashedTransactions<RuntimeTransaction<SanitizedTransaction>>;

fn sanitize(
    entries: Vec<Entry>,
    pool: &ThreadPool,
    per_entry: bool,
) -> Result<Vec<Validated>, TransactionError> {
    if per_entry {
        entries
            .into_iter()
            .map(|entry| {
                let count = entry.transactions.len();
                validate_and_hash_transactions(
                    vec![entry],
                    count,
                    pool,
                    support::validate_transaction,
                )
            })
            .collect()
    } else {
        let count = entries.iter().map(|e| e.transactions.len()).sum();
        validate_and_hash_transactions(entries, count, pool, support::validate_transaction)
            .map(|v| vec![v])
    }
}

fn outputs(validated: &[Validated]) -> Vec<(Hash, Vec<Signature>)> {
    validated
        .iter()
        .flat_map(|v| &v.entries)
        .flat_map(|e| match e {
            EntryType::Tick(hash) => vec![(*hash, vec![])],
            EntryType::Transactions(txs) => txs
                .iter()
                .map(|tx| (*tx.message_hash(), tx.signatures().to_vec()))
                .collect(),
        })
        .collect()
}

fn signatures_ok(validated: &[Validated], pool: &ThreadPool) -> bool {
    pool.install(|| {
        validated
            .par_iter()
            .all(|v| v.unverified_signatures.verify().is_ok())
    })
}

fn cpu_seconds() -> f64 {
    let mut usage = std::mem::MaybeUninit::<libc::rusage>::uninit();
    // SAFETY: getrusage initializes the supplied rusage on success; checked below.
    assert_eq!(
        unsafe { libc::getrusage(libc::RUSAGE_SELF, usage.as_mut_ptr()) },
        0
    );
    let usage = unsafe { usage.assume_init() };
    let seconds = |t: libc::timeval| t.tv_sec as f64 + t.tv_usec as f64 / 1e6;
    seconds(usage.ru_utime) + seconds(usage.ru_stime)
}

fn measure(width: usize, round: usize, name: &str, mut operation: impl FnMut()) {
    const ITERATIONS: usize = 10;
    operation(); // warm every path before sampling
    let cpu = cpu_seconds();
    let wall = Instant::now();
    for _ in 0..ITERATIONS {
        operation();
    }
    let wall_ms = wall.elapsed().as_secs_f64() * 1000.0 / ITERATIONS as f64;
    let cpu_ms = (cpu_seconds() - cpu) * 1000.0 / ITERATIONS as f64;
    println!("{round},{width},{name},{ITERATIONS},{wall_ms:.6},{cpu_ms:.6}");
}

fn main() {
    let start = Hash::default();
    let keypair = Keypair::new_from_array([7; 32]);
    let mut hash = start;
    let mut entries = Vec::new();
    for i in 0..64 {
        let txs = support::signed_transfers(&keypair, i * 32..(i + 1) * 32);
        let entry = Entry::new(&hash, 1, txs);
        hash = entry.hash;
        entries.push(entry);
        let tick = Entry::new(&hash, 12_499, vec![]);
        hash = tick.hash;
        entries.push(tick);
    }
    let prepared = entries_to_verification_data(&entries);
    let mut tick_hash_count = 0;
    assert!(entries.verify_tick_hash_count(&mut tick_hash_count, 12_500));
    assert_eq!(entries.tick_count(), 64);
    assert_eq!(tick_hash_count, 0);
    println!(
        "# 128 entries, 64 ticks, 800000 PoH hashes, 2048 distinct signed legacy transfers; final_hash={hash}"
    );
    println!("round,width,phase,iterations,wall_ms,cpu_ms");
    let pools: Vec<_> = [1, 2, 4]
        .into_iter()
        .map(|width| ThreadPoolBuilder::new().num_threads(width).build().unwrap())
        .collect();
    let reference = outputs(&sanitize(entries.clone(), &pools[0], false).unwrap());
    for pool in &pools {
        assert!(entries.verify(&start, pool).status());
        assert!(verify_entries_cpu_in_pool(&prepared, &start, pool).status());
        let mut bad_hash = entries.clone();
        bad_hash[64].hash = Hash::default();
        assert!(!bad_hash.verify(&start, pool).status());
        assert!(
            !verify_entries_cpu_in_pool(&entries_to_verification_data(&bad_hash), &start, pool)
                .status()
        );
        // Rebuild the entire chain around a bad signature: PoH remains valid,
        // so only actual signature verification can reject this control.
        let mut bad_sig = entries.clone();
        bad_sig[64].transactions[0].signatures[0] = Signature::default();
        let mut prev = start;
        for e in &mut bad_sig {
            *e = next_versioned_entry(&prev, e.num_hashes, std::mem::take(&mut e.transactions));
            prev = e.hash;
        }
        assert!(bad_sig.verify(&start, pool).status());
        for per_entry in [true, false] {
            let valid = sanitize(entries.clone(), pool, per_entry).unwrap();
            assert_eq!(outputs(&valid), reference);
            assert!(signatures_ok(&valid, pool));
            let invalid = sanitize(bad_sig.clone(), pool, per_entry).unwrap();
            assert!(!signatures_ok(&invalid, pool));
            let mut malformed = entries.clone();
            malformed[64].transactions[0].signatures.clear();
            assert!(sanitize(malformed, pool, per_entry).is_err());
        }
    }
    println!(
        "# controls passed at every width: hash corruption, valid-PoH invalid signature, missing signature; ordered message hashes/signatures/tick hashes identical"
    );
    // Rotate width order to reduce systematic warm-up/drift bias.
    for round in 0..3 {
        for index in 0..3 {
            let pool = &pools[(round + index) % 3];
            let width = pool.current_num_threads();
            measure(width, round, "poh_slice", || {
                assert!(black_box(&entries).verify(&start, pool).status())
            });
            measure(width, round, "poh_prepared", || {
                assert!(verify_entries_cpu_in_pool(black_box(&prepared), &start, pool).status())
            });
            for per_entry in [true, false] {
                let mode = if per_entry { "per_entry" } else { "batch" };
                measure(width, round, &format!("sanitize_{mode}"), || {
                    black_box(sanitize(black_box(&entries).clone(), pool, per_entry).unwrap());
                });
                let validated = sanitize(entries.clone(), pool, per_entry).unwrap();
                measure(width, round, &format!("signatures_{mode}"), || {
                    assert!(signatures_ok(black_box(&validated), pool))
                });
            }
            measure(width, round, "full_batch", || {
                assert!(entries.verify(&start, pool).status());
                let validated = sanitize(entries.clone(), pool, false).unwrap();
                assert!(signatures_ok(&validated, pool));
            });
        }
    }
}
