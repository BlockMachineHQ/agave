# Warm native SVM call overhead — 2026-09-13

## Decision

**Do not extract a new execution API or optimize production code from this
experiment.** Small batches have a reproducible benefit in these native fixtures,
but the measured five-builtin cache clone is only about 39–42 ns/call. The larger
singleton/batch difference also includes native account-cache reuse and result
allocation amortization; it is not an isolated, reusable setup cost.

This is local, width-one load/execute research for singleton consumers such as
BlockMachine. It does not establish that production replay can batch transactions
across scheduling, commit, failure, or bank boundaries. No scheduler, native
protocol path, dependency pin, or BM runtime implementation was changed.

## Provenance and reproduction

- Native base: `930d26acab1139e5bc0a8535298e15946fc772ca`, task branch
  `bm-4.2.1-night-svm`. This is the operator-selected Agave base, not a claim that
  an existing BM checkout already consumes this revision.
- Apple M3 Max, aarch64 macOS, 36 GiB RAM; Rust 1.96.1
  (`31fca3adb`, 2026-06-26). **Real SBF execution uses the native interpreter on
  this architecture; these are not Linux/x86 JIT timings.**
- Cargo.lock SHA-256:
  `24572676ffe5c951e80ed4111c1f791972b2f98a8638467567fe9952f3a00c60`.
- Checked-in `write_to_account_program.so` SHA-256:
  `b8917369c7daab26c5cfe136dea108786839261c5e44b2cc480c6d86a5f4fc47`.
- Separate worktree-local `target-night-svm`, at most two build jobs, all Cargo
  operations offline/locked. Benchmark processes run sequentially after builds.
  Other operator-local activity was not stopped or controlled; no CPU affinity
  or frequency control was applied.

From the Agave worktree root:

```sh
CC=/usr/bin/clang CXX=/usr/bin/clang++ CFLAGS=-fno-lto \
CARGO_PROFILE_RELEASE_LTO=false \
cargo test --offline --locked --release -j2 --target-dir target-night-svm \
  -p solana-svm --test integration_test --no-run

# Use the executable path printed by Cargo (hash varies with toolchain/config).
python3 svm/tests/call_overhead/summarize.py \
  target-night-svm/release/deps/integration_test-bdca8f2243256485 \
  svm/tests/call_overhead/new-run
```

The runner starts only the ignored benchmark, with `--exact --nocapture
--test-threads=1`, from `svm/` as required by the existing program-loading fixture.
It retains raw stdout/stderr in `.txt` and paired descriptive statistics in
`.json`. Choose a fresh output prefix to retain earlier observations.

The default local release link failed with an LLVM summary-version mismatch
(version 14 versus Apple linker's supported 1–12). Disabling Rust LTO alone also
failed. Explicit Apple C/C++ compilers and `CFLAGS=-fno-lto`, together with disabled
Rust LTO, produced the measured executable. These are local build settings, not
repository dependency or profile changes.

## Experiment

The ignored test reuses `SvmTestEntry`, `SvmTestEnvironment`, `MockBankCallback`,
native builtin registration, and the checked-in SBF program. It implements no
transaction processor, scheduler, fee logic, loader, or account store of its own.

Each workload contains the same **256 sanitized independent transactions** in
each arm. Every transaction has its own funded payer and writable target. The
only shared accounts are read-only program/loader accounts:

1. Native system-program transfer of 123 lamports to an existing account.
2. Real loader-v3 SBF instruction writing byte `100` into a one-byte,
   program-owned account (`write-to-account`, instruction `[1]`).

There are five registered builtins, prefilled sysvars, the native integration
fixture's all-enabled SVM feature set, default rent and compute budget, and its
custom runtime environment. Log, return-data, CPI, and balance recording are off;
deployment-slot checks are on, matching the relevant BM recording/check settings.
The feature set/custom runtime are synthetic fixture inputs, not captured mainnet
or exact BM bank inputs.

Four untimed passes warm all transactions/programs. Each timing sample executes
64 passes of the 256 transactions (16,384 executions), split into calls of 1, 2,
4, 8, or 16 transactions. All calls are sequential on one test thread. Twelve
rounds per process pair each small-batch sample with a fresh singleton sample;
even rounds run AB in ascending batch order, odd rounds BA in descending order.
Two fresh processes repeat the experiment (384 timing samples total).

The timer surrounds only the native call, including its internal cleanup before
return. Sanitation, signing, input-check-vector cloning, returned-output
destruction, and metric aggregation are outside that timer. Stopwatch overhead
is included and not corrected. Each reported sample is a mean ns/transaction
over its executions; medians and standard deviations below are across those
sample means, **not transaction latency percentiles or confidence intervals**.

The callback delegates account fetches to the native fixture's in-memory map
and uncontended read lock. Timed inspection is a no-op; a separate diagnostic
pass records inspections and fetch counts. No returned account changes are
committed into the backing image: disjoint writable accounts make commits
irrelevant to subsequent inputs in a pass, and every pass deliberately replays
the identical image. Output equality includes all account changes a consumer
would receive, but this measures neither commit costs nor rejection restoration.

## Measurements

All timings below are ns/transaction. `saved ± SD` is the median paired
singleton-minus-batched difference and the sample standard deviation of the 12
paired differences. The separate singleton median belongs to that batch's
adjacent controls, rather than one reused baseline.

| Run | Workload | Batch | Singleton median | Batched median | Saved median ± SD | Paired reduction median |
|---|---|---:|---:|---:|---:|---:|
| 1 | builtin transfer | 2 | 1818.7 | 1685.4 | 130.7 ± 33.0 | 7.23% |
| 1 | builtin transfer | 4 | 1830.1 | 1628.3 | 197.3 ± 16.9 | 10.84% |
| 1 | builtin transfer | 8 | 1837.9 | 1600.3 | 237.0 ± 15.6 | 12.87% |
| 1 | builtin transfer | 16 | 1834.1 | 1579.2 | 253.1 ± 14.9 | 13.82% |
| 2 | builtin transfer | 2 | 1852.5 | 1706.0 | 152.6 ± 30.0 | 8.20% |
| 2 | builtin transfer | 4 | 1834.8 | 1626.1 | 212.3 ± 18.8 | 11.50% |
| 2 | builtin transfer | 8 | 1834.7 | 1600.2 | 229.7 ± 25.3 | 12.52% |
| 2 | builtin transfer | 16 | 1842.4 | 1578.9 | 262.5 ± 12.2 | 14.30% |
| 1 | SBF write | 2 | 7566.3 | 7242.4 | 324.2 ± 86.2 | 4.29% |
| 1 | SBF write | 4 | 7636.7 | 7154.8 | 480.4 ± 75.1 | 6.28% |
| 1 | SBF write | 8 | 7658.3 | 7092.2 | 564.3 ± 79.0 | 7.38% |
| 1 | SBF write | 16 | 7600.2 | 7037.8 | 565.6 ± 66.3 | 7.46% |
| 2 | SBF write | 2 | 7449.9 | 7168.4 | 266.4 ± 52.8 | 3.59% |
| 2 | SBF write | 4 | 7449.0 | 7008.0 | 440.8 ± 58.2 | 5.93% |
| 2 | SBF write | 8 | 7441.7 | 6968.3 | 480.6 ± 46.3 | 6.46% |
| 2 | SBF write | 16 | 7427.8 | 6902.0 | 532.7 ± 53.9 | 7.13% |

All 192 paired differences were positive. The smallest was 39.9 ns/transaction
(run 1, SBF batch 2). Full min/max, means, standard deviations, and paired
percentages are in [run 1 JSON](2026-09-13-run1.json) and
[run 2 JSON](2026-09-13-run2.json); raw native timing rows are in
[run 1](2026-09-13-run1.txt) and [run 2](2026-09-13-run2.txt).

### Setup components and attribution limits

| Probe | Run 1 builtin / SBF median | Run 2 builtin / SBF median |
|---|---:|---:|
| Actual empty native call, including returned-output drop | 68.3 / 64.2 ns | 70.4 / 64.3 ns |
| Native five-builtin cache clone + uncontended read lock + drop | 39.4 / 40.0 ns | 39.4 / 41.7 ns |

The clone probe uses `ProgramCacheForTxBatch` and the five actual registered
builtin entries from the fixture, in its own uncontended `RwLock`. It is a
component microbenchmark, not an instrumented region of the production call.
An empty call has no account-cache capacity or output-vector capacity and is
not representative of all singleton setup. These component times cannot be
subtracted from the full-call delta to partition it causally.

Native `ValidateFeesUs`, `LoadUs`, `ExecuteUs`, and `ProgramCacheUs` counters are
retained per sample. They use integer microseconds per internal timed scope;
many operations round down to zero, especially builtin execution subscopes.
`ExecuteUs` includes nested cache work, so the counters are not additive.
For example, run 1 SBF batch-8 controls record about 6198 ns/transaction in
`ExecuteUs`, versus 6132 ns batched; their wall-call difference is about 564 ns.
That does **not** establish a 498 ns pure-setup hotspot.

Allocator event counts/bytes were **not measured**. Source inspection identifies
per-call account-loader capacity allocation, result-vector allocation, and the
builtin-cache clone; the benchmark measures their combined path but does not
assign time or bytes to individual allocator sites. No allocator interposition
was installed into the native integration-test binary.

## Equivalence and side effects

Before timing, every batch size is checked against the warmed singleton arm:

- All 256 transactions succeed. Full `LoadedTransaction` equality covers account
  keys and all account fields/bytes, touched flags, fees, rollback accounts,
  compute budget, and loaded-account data size.
- Full `TransactionExecutionDetails` equality covers status, CU, account deltas,
  and configured recording outputs. Independently assert exact payer debits,
  transfer credits, and SBF byte writes; both arms cannot pass by doing nothing.
- Account inspection histories match exactly per address, including preimage
  bytes, writable flags, order for each address, and multiplicity. The fixture
  stores histories per address; global cross-address inspection order is not
  asserted.
- Program modifications are empty; warmed cache keys/entry pointers and
  compilation counts are unchanged. ELF-load, verification, and JIT-load timing
  counters are zero. Native program-use increments are 256 for every arm.
- Measured global cache-hit deltas also match: 1280 for the builtin workload and
  1536 for SBF. Time-based program statistics are not expected to match exactly.
- The backing account map remains exactly unchanged. The full equal returned
  account/touched outputs define equal potential writebacks for these successful
  fixtures; there is no BM commit implementation in this benchmark.

**Not all callback activity is identical:** account-loader reuse deliberately
reduces backing fetches. Per 256 transactions:

| Batch | Builtin reads | SBF reads |
|---:|---:|---:|
| 1 | 768 | 1024 |
| 2 | 640 | 768 |
| 4 | 576 | 640 |
| 8 | 544 | 576 |
| 16 | 528 | 544 |

Thus these arms have exact tested transaction/inspection outputs, but are not
identical traces of callback invocations. Lower read counts are an observed
part of the batching benefit even with all inputs in memory.

## Validation and remaining scope

- Both retained release benchmark runs passed all equivalence assertions.
- Native `integration_test`: **48 passed, 1 manual benchmark ignored**.
- Narrow release Clippy with `-D warnings`: passed. Its first run identified the
  benchmark's constant debug-build guard; a targeted lint allowance keeps that
  guard runtime-only so ordinary debug test compilation remains possible.
- Stable rustfmt check and `git diff --check`: passed. Nightly-only formatting
  options emit warnings because no nightly toolchain is installed.
- The repository-wide pre-push CI scripts were inspected but not executed:
  they include remote fetching/auditing, nightly tooling, and whole-workspace
  checks outside this offline native benchmark scope. No full CI pass is claimed.

The recorded benchmark binary SHA-256 is
`8b8d7fcc99a2474acb7876de022308e4477c6710fa89179f6305b15586fe954f`.
The only Rust edit after its measurement was the lint-only guard allowance
described above; no executable benchmark logic or runtime source was changed.

The measured native-owned clone is small and already uses shared native code.
There is no measured, simply removable shared setup hotspot here that warrants
an API split. Larger representative SBF workloads, native allocator attribution,
and any exact-input Linux/JIT or production scheduling experiment would be
separate evidence, not conclusions supplied by this local fixture.
