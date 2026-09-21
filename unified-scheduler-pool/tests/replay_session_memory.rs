//! Bounded Rust-heap measurement with all input payloads allocated before the
//! measurement window. This does not measure RSS, stacks or SVM/account storage.
use {
    solana_clock::Slot,
    solana_hash::Hash,
    solana_keypair::Keypair,
    solana_pubkey::Pubkey,
    solana_runtime_transaction::runtime_transaction::RuntimeTransaction,
    solana_svm_timings::ExecuteTimings,
    solana_system_transaction::transfer,
    solana_transaction_error::TransactionResult,
    solana_unified_scheduler_logic::Task,
    solana_unified_scheduler_pool::replay_session::{
        ReplaySession, ReplaySessionContext, ReplaySessionHandler,
    },
    std::{
        alloc::{GlobalAlloc, Layout, System},
        sync::{
            Arc, Barrier,
            atomic::{AtomicUsize, Ordering},
        },
    },
};

struct CountHeap;
static LIVE: AtomicUsize = AtomicUsize::new(0);
static PEAK: AtomicUsize = AtomicUsize::new(0);

fn allocated(bytes: usize) {
    let live = LIVE
        .fetch_add(bytes, Ordering::SeqCst)
        .saturating_add(bytes);
    PEAK.fetch_max(live, Ordering::SeqCst);
}

unsafe impl GlobalAlloc for CountHeap {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let result = unsafe { System.alloc(layout) };
        if !result.is_null() {
            allocated(layout.size());
        }
        result
    }
    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        LIVE.fetch_sub(layout.size(), Ordering::SeqCst);
        unsafe { System.dealloc(ptr, layout) };
    }
    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, size: usize) -> *mut u8 {
        let result = unsafe { System.realloc(ptr, layout, size) };
        if !result.is_null() {
            if size >= layout.size() {
                allocated(size.saturating_sub(layout.size()));
            } else {
                LIVE.fetch_sub(layout.size().saturating_sub(size), Ordering::SeqCst);
            }
        }
        result
    }
}

#[global_allocator]
static ALLOCATOR: CountHeap = CountHeap;

#[derive(Debug, Clone)]
struct Context {
    gate: Arc<Barrier>,
    completed: Arc<AtomicUsize>,
    fail: bool,
    panic: bool,
}
impl ReplaySessionContext for Context {
    fn slot(&self) -> Slot {
        1
    }
}
#[derive(Debug)]
struct Handler;
impl ReplaySessionHandler for Handler {
    type Context = Context;
    type Services = ();
    fn handle(
        result: &mut TransactionResult<()>,
        _timings: &mut ExecuteTimings,
        context: &Context,
        task: &Task,
        _services: &(),
    ) {
        if task.task_id() == 0 {
            context.gate.wait();
        }
        context.completed.fetch_add(1, Ordering::SeqCst);
        assert!(!context.panic, "controlled resource-fixture panic");
        if context.fail {
            *result = Err(solana_transaction_error::TransactionError::AccountNotFound);
        }
    }
}

#[test]
fn stalled_dependency_queue_heap_fits_reserved_metadata_envelope() {
    for count in [256usize, 2048] {
        for width in [1, 4] {
            let payer = Keypair::new();
            let transactions: Vec<_> = (0..count)
                .map(|_| {
                    RuntimeTransaction::from_transaction_for_tests(transfer(
                        &payer,
                        &Pubkey::new_unique(),
                        1,
                        Hash::default(),
                    ))
                })
                .collect();
            let context = Context {
                gate: Arc::new(Barrier::new(2)),
                completed: Arc::new(AtomicUsize::new(0)),
                fail: false,
                panic: false,
            };
            let base = LIVE.load(Ordering::SeqCst);
            PEAK.store(base, Ordering::SeqCst);
            let session = ReplaySession::<Handler>::new(1, context.clone(), (), width)
                .with_fifo_initial_capacity(0);
            for (id, tx) in transactions.into_iter().enumerate() {
                session.schedule_execution(tx, id as u128).unwrap();
            }
            context.gate.wait();
            let (result, idle) = session.finish();
            result.0.unwrap();
            let mut idle = idle.unwrap();
            assert_eq!(context.completed.load(Ordering::SeqCst), count);
            let peak = PEAK.load(Ordering::SeqCst).saturating_sub(base);
            let reservation = 65536usize
                .saturating_add(count.saturating_mul(2048))
                .saturating_add(count.saturating_mul(3).saturating_mul(2048));
            assert!(
                peak <= reservation,
                "native heap {peak} exceeds reservation {reservation}"
            );
            assert!(idle.is_overgrown(0));
            idle.clear_usage_queues();
            assert!(!idle.is_overgrown(0));
            drop(idle);
            println!(
                "session_heap tasks={count} handlers={width} peak_delta={peak} reserved={reservation}"
            );
        }
    }
    for panicking in [false, true] {
        let base = LIVE.load(Ordering::SeqCst);
        for _ in 0..16 {
            let payer = Keypair::new();
            let context = Context {
                gate: Arc::new(Barrier::new(2)),
                completed: Arc::new(AtomicUsize::new(0)),
                fail: true,
                panic: panicking,
            };
            let session = ReplaySession::<Handler>::new(2, context.clone(), (), 2)
                .with_fifo_initial_capacity(0);
            for id in 0..128 {
                let tx = RuntimeTransaction::from_transaction_for_tests(transfer(
                    &payer,
                    &Pubkey::new_unique(),
                    1,
                    Hash::default(),
                ));
                session.schedule_execution(tx, id).unwrap();
            }
            context.gate.wait();
            let outcome =
                std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| session.finish()));
            if panicking {
                assert!(outcome.is_err());
            } else {
                let (result, idle) = outcome.unwrap();
                assert!(result.0.is_err());
                assert!(idle.is_none());
            }
        }
        let retained = LIVE.load(Ordering::SeqCst).saturating_sub(base);
        println!("session_abort_heap attempts=16 panicking={panicking} retained_delta={retained}");
        assert!(
            retained < 65536,
            "aborted sessions retained task/usage-queue ownership"
        );
    }
}
