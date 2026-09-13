# Native entry verification: same-input pool-width experiment

## Decision

Existing native verification parallelizes this fixture across widths 1/2/4,
reducing isolated wall time while consuming approximately the same CPU. This is
**not evidence of live replay acceleration**. The operator-provided P1 premise
was approximately 29% PoH SHA verification CPU but only 0.18 ms exposed join;
overlapped work can consume CPU without delaying replay. Neither premise was
remeasured here.

The next performance-sharing candidate is to use existing
`validate_and_hash_transactions` across multiple entries where BM's address
loading/evidence context permits it. Do not rewrite the native scheduler on this
evidence. Pure tick-structure validation is a separate small code-sharing
candidate, described below; it was inspected but not extracted in this change.

## Provenance and workload

- Native worktree: `agave-night-verification`, branch
  `bm-4.2.1-night-verification`, based on
  `930d26acab1139e5bc0a8535298e15946fc772ca`.
- Inspected BM worktree at `d2a4fd4`, pin
  `39fe69b77c82bd80b86a34ca5ad49ba2eaeedf72`. Native `entry/` and
  `ledger/src/blockstore_processor.rs` have no diff between that pin and this
  experiment's base. No native AGENTS.md or CLAUDE.md was present.
- Local Apple M3 Max, 14 logical CPUs, aarch64 macOS 26.6.2 (25G83),
  rustc 1.96.1 (31fca3adb). Default bench/release optimization and thin LTO;
  repository target rustflags, no target-cpu override or affinity.
- Existing native `verify_signatures` signed-transfer fixture and sanitation
  callback moved into `benches/support/mod.rs` and used by both benches.
  This experiment fixes the keypair seed to `[7; 32]` and uses distinct amounts
  0..2048. It runs real native entry hashes, message sanitation/hash creation,
  and Ed25519 signature verification, not mirrored verification algorithms.
- 128 entries: 64 transaction entries of 32 legacy transfers each, alternating
  with 64 ticks; 1 + 12,499 declared PoH hashes per pair, 800,000 total,
  plus native signature-Merkle work. Final entry hash:
  `2NzMWVosMeAcv9KuRNhbCySeK7dD534hFuj92urnCFX8`.
- These are synthetic signed transfers, not a captured P1/mainnet block. No v0
  lookup tables, votes, precompiles, Bank execution, account loading, storage,
  page faults, or competing execution workers are modeled. The sequential
  `full_batch` timing is not native async replay orchestration.

## Method and controls

All widths reuse identical entries and signatures. Persistent pools are built
before measurement. Three rounds rotate width order (1/2/4, 2/4/1, 4/1/2); each
phase warms once, then measures ten operations. Wall time uses `Instant`; CPU
is process user+system time from `getrusage(RUSAGE_SELF)`, including pool workers.
The other pools remain idle. No compilation was run concurrently by this task.
Measurements are short local samples, not confidence intervals or a controlled
host-contention experiment.

- `poh_slice`: native `EntrySlice::verify`, including verification-data creation.
- `poh_prepared`: native `verify_entries_cpu_in_pool`, with the same verification
  data prepared outside timing. This deliberately isolates repeated setup;
  preparing once is not free for a one-shot block.
- `sanitize_per_entry` / `sanitize_batch`: same native callback and same input
  clone, including output destruction, one entry per call vs the whole slice.
  Both retain all transactions. The batch crosses the native 200-tx threshold;
  the 32-tx per-entry calls do not. No signatures are checked in this phase,
  matching the API's deferred-verification contract.
  The experiment retains tick entries in both sanitation modes to compare the
  whole native output stream. BM's caller skips empty tick entries, so this
  per-entry mode includes 64 cheap calls that BM does not make; it is not an
  exact timing of BM's wrapper.
- `signatures_*`: actual native verification of every deferred signature, with
  an outer pool install and parallel batch traversal matching BM's shape.
- `full_batch`: PoH + batch sanitation + signatures, sequential and including
  input clone/output destruction. Tick hash counts and tick count are validated
  on the fixed fixture outside timing, not repeated inside this measurement.

Before timing, every width asserts valid PoH on both APIs and identical ordered
message hashes, signatures and tick hashes across both sanitation modes. Every
width also rejects a corrupted entry hash through both PoH APIs, rejects a
default signature through both sanitation modes' real signature verification,
and rejects a missing signature during sanitation. The bad-signature fixture's
entire PoH chain is rebuilt and asserted valid, so a PoH failure cannot mask a
signature bypass. Valid signature verification is asserted before and during
signature/full-path measurements. Negative controls are not timed.

## Results

Median of three per-round means, milliseconds per workload. Each cell is
**wall / process CPU**; raw samples are in
[`2026-09-13-verification-pool.csv`](2026-09-13-verification-pool.csv).

| Native path | Width 1 | Width 2 | Width 4 |
|---|---:|---:|---:|
| PoH slice, including preparation | 119.699 / 119.573 | 60.138 / 119.750 | 30.393 / 120.352 |
| PoH prepared | 119.412 / 119.273 | 60.056 / 119.628 | 30.266 / 120.399 |
| Sanitation per entry | 1.157 / 1.155 | 1.158 / 1.157 | 1.143 / 1.143 |
| Sanitation batch | 1.185 / 1.192 | 0.750 / 1.248 | 0.520 / 1.370 |
| Signatures per entry | 65.762 / 65.678 | 32.983 / 65.789 | 16.974 / 67.584 |
| Signatures batch | 65.623 / 65.555 | 33.108 / 65.954 | 16.685 / 66.227 |
| Full sequential batch | 186.419 / 186.226 | 94.137 / 187.075 | 47.859 / 188.419 |

PoH's isolated width-4 wall reduction is about 3.94x, with essentially unchanged
CPU. Prepared-data differences are small relative to PoH work and are not a
demonstrated material optimization for this workload. At width 4, batch sanitation
saves about 0.62 ms vs per-entry sanitation, at about 0.23 ms more process CPU.
At width 1, batching is slightly slower. These effects do not establish improved
BM throughput or justify increasing its verification pool under execution load.

## Source inspection and specific sharing recommendation

1. **Use the existing multi-entry sanitation API before adding an API.**
   `entry/src/entry.rs::validate_and_hash_transactions` allocates one combined
   result below 200 transactions, and above that uses ordered Rayon collection
   across entries followed by signature-vector concatenation. Transactions within
   each entry remain serial. `ledger/src/blockstore_processor.rs` already passes
   the fetched entry batch to it. BM `feed.rs::sanitize_entry` instead constructs
   a one-entry vector/result per call, preventing cross-entry parallelism even
   when the entire block exceeds the threshold. There is no per-entry pool
   construction to remove; below 200 there is not even a pool install.

   BM's callback carries an entry-sensitive `StoreAddressLoader`, fatal evidence,
   and a serial transaction counter. Blindly replacing `vec![entry]` with all
   entries would lose that association. Its current caller already sanitizes all
   non-tick entries before execution, so there is an existing batch boundary to
   investigate; retain stable loading semantics there. If context prevents direct
   reuse, the specific next native API proposal is an **indexed callback variant** carrying entry and
   transaction indices into the existing sanitation implementation, with the
   existing public API as a wrapper. It must be used by native production and BM,
   preserve ordered outputs and error behavior, and test failures in multiple
   entries. This fixture measures neither ALT consistency nor failure-selection
   ordering under multiple simultaneous errors.

2. **Reuse the existing prepared PoH API only when ownership calls for it.**
   `EntrySlice::verify` creates signature-vector verification data then installs
   work into the supplied pool. Native ledger production already prepares the
   data once before spawning and calls `verify_entries_cpu` inside the pool.
   BM could use the same preparation/ownership boundary without copying hashing
   logic. It still verifies each block once; moving preparation out of the worker
   shifts work onto the serial prefix and does not eliminate that work. The
   prepared benchmark is not evidence to make this change for performance.

3. **Small pure tick-validation export is a fidelity-sharing candidate.**
   Extract the ordered decision logic from native `verify_ticks` into a native
   production-used function accepting actual tick height, max height, slot-full,
   hashes-per-tick, resolved Alpenglow tick mode, entries, and mutable carried
   tick-hash count. Keep Bank/MigrationStatus access and contextual logging in the
   native wrapper. Preserve TooManyTicks, TooFewTicks, TrailingEntry,
   InvalidLastTick, and InvalidTickHashCount precedence, partial-segment mutation,
   zero/None hash-count semantics, and Alpenglow's all-num_hashes-equal-one rule.
   BM supplies its complete-slot heights; it must not substitute guessed defaults
   or silently switch between its saturating height arithmetic and native `+`.
   Reuse native ledger tick fixtures against the extracted production path and
   add a partial-segment/error-state differential before integration. No scheduler
   or storage work is needed for this candidate. It has no measured speed claim.

## Validation and reproduction

On this Mac, Homebrew Clang 23 native objects caused Apple linker 21 to fail with
`Invalid summary version 14` and crash. Disabling Rust release LTO alone did not
fix it. Selecting Apple Clang for C/C++ fixed linking with the repository's
original thin-LTO release settings. No source/dependency pin workaround was used.

```sh
CC=/usr/bin/clang CXX=/usr/bin/clang++ cargo bench --locked -p solana-entry --bench verification_pool
CC=/usr/bin/clang CXX=/usr/bin/clang++ cargo test --locked --release -p solana-entry --lib
CC=/usr/bin/clang CXX=/usr/bin/clang++ cargo bench --locked -p solana-entry --bench verify_signatures -- --test
```

- New benchmark: all controls and all 63 timing samples completed.
- Native entry tests: 25 passed, including PoH fuzz, tick hash counts, signature
  failures, and transaction reorder rejection.
- Original signature benchmark: all 10 Criterion test-mode cases passed after
  fixture extraction.
- The only lockfile change records the benchmark's direct dev dependency on the
  already-pinned workspace `libc`; no dependency versions/revisions changed.
- No host access, BM source edits, integration pin changes, or production
  algorithm changes were made by this experiment.
