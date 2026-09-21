//! Exercise the public engine without constructing a Bank or AccountsDb.
use {
    crossbeam_channel::{Receiver, Sender, bounded},
    solana_clock::Slot,
    solana_hash::Hash,
    solana_keypair::Keypair,
    solana_pubkey::Pubkey,
    solana_runtime_transaction::runtime_transaction::RuntimeTransaction,
    solana_svm_timings::ExecuteTimings,
    solana_system_transaction::transfer,
    solana_transaction::sanitized::SanitizedTransaction,
    solana_transaction_error::{TransactionError, TransactionResult},
    solana_unified_scheduler_logic::Task,
    solana_unified_scheduler_pool::replay_session::{
        ReplaySession, ReplaySessionContext, ReplaySessionHandler,
    },
    std::{
        collections::HashSet,
        sync::{Arc, Mutex},
        thread::{self, ThreadId},
        time::Duration,
    },
};

const WAIT: Duration = Duration::from_secs(10);

#[derive(Debug, Default)]
struct State {
    commits: Mutex<Vec<(u64, u128)>>,
}

#[derive(Clone, Debug)]
struct Context {
    generation: u64,
    state: Arc<State>,
    fail_after_commit: Option<u128>,
    first_task_gate: Option<(Sender<()>, Receiver<()>)>,
}

impl ReplaySessionContext for Context {
    fn slot(&self) -> Slot {
        // Deliberately identical slots: the engine must propagate actual context,
        // not derive execution identity from the diagnostic slot number.
        42
    }
}

#[derive(Debug)]
struct Handler;

impl ReplaySessionHandler for Handler {
    type Context = Context;
    type Services = Arc<Mutex<HashSet<ThreadId>>>;

    fn handle(
        result: &mut TransactionResult<()>,
        _timings: &mut ExecuteTimings,
        context: &Context,
        task: &Task,
        threads: &Self::Services,
    ) {
        threads.lock().unwrap().insert(thread::current().id());
        let id = task.task_id();
        if id == 0
            && let Some((started, release)) = &context.first_task_gate
        {
            started.send(()).unwrap();
            release.recv_timeout(WAIT).unwrap();
        }
        let mut commits = context.state.commits.lock().unwrap();
        // All transactions conflict on the payer. A task must observe every
        // preceding commit before the shared scheduler releases it to a handler.
        assert_eq!(commits.len(), id as usize);
        commits.push((context.generation, id));
        if context.fail_after_commit == Some(id) {
            *result = Err(TransactionError::WouldExceedMaxBlockCostLimit);
        }
    }
}

fn transaction(payer: &Keypair, index: u64) -> RuntimeTransaction<SanitizedTransaction> {
    RuntimeTransaction::from_transaction_for_tests(transfer(
        payer,
        &Pubkey::new_unique(),
        index.checked_add(1).unwrap(),
        Hash::default(),
    ))
}

fn context(generation: u64) -> Context {
    Context {
        generation,
        state: Arc::new(State::default()),
        fail_after_commit: None,
        first_task_gate: None,
    }
}

#[test]
fn bank_free_dependency_visibility_and_same_slot_session_reuse() {
    for width in [1, 4] {
        let threads = Arc::new(Mutex::new(HashSet::new()));
        let payer = Keypair::new();
        let mut current = context(0);
        let mut session =
            ReplaySession::<Handler>::new(17, current.clone(), threads.clone(), width);
        for generation in 0..32 {
            let count = if generation % 3 == 1 { 0 } else { 8 };
            for id in 0..count {
                session
                    .schedule_execution(transaction(&payer, id), id as u128)
                    .unwrap();
            }
            let (result, idle) = session.finish();
            result.0.unwrap();
            assert_eq!(
                *current.state.commits.lock().unwrap(),
                (0..count)
                    .map(|id| (generation, id as u128))
                    .collect::<Vec<_>>()
            );
            let idle = idle.expect("healthy sessions are reusable");
            assert_eq!(idle.id(), 17);
            assert!(idle.is_overgrown(0));
            // Reinitialization retains the same worker threads, including across
            // empty sessions and distinct execution identities at the same slot.
            assert!(threads.lock().unwrap().len() <= width);
            if generation == 31 {
                drop(idle);
                break;
            }
            current = context(generation + 1);
            session = idle.resume(current.clone());
        }
    }
}

#[test]
fn post_commit_error_retires_workers_without_executing_dependents() {
    let (started_tx, started_rx) = bounded(1);
    let (release_tx, release_rx) = bounded(1);
    let mut context = context(0);
    context.fail_after_commit = Some(1);
    context.first_task_gate = Some((started_tx, release_rx));
    let state = context.state.clone();
    let session =
        ReplaySession::<Handler>::new(18, context, Arc::new(Mutex::new(HashSet::new())), 4);
    let payer = Keypair::new();
    for id in 0..8 {
        session
            .schedule_execution(transaction(&payer, id), id as u128)
            .unwrap();
    }
    started_rx.recv_timeout(WAIT).unwrap();
    release_tx.send(()).unwrap();
    let (result, idle) = session.finish();
    assert_eq!(
        result.0,
        Err(TransactionError::WouldExceedMaxBlockCostLimit)
    );
    assert!(idle.is_none());
    assert_eq!(*state.commits.lock().unwrap(), [(0, 0), (0, 1)]);
}

#[test]
fn idle_context_remains_owned_until_retirement() {
    let context = context(0);
    let weak = Arc::downgrade(&context.state);
    let session =
        ReplaySession::<Handler>::new(19, context, Arc::new(Mutex::new(HashSet::new())), 2);
    let (result, idle) = session.finish();
    result.0.unwrap();
    let idle = idle.unwrap();
    assert!(
        weak.upgrade().is_some(),
        "idle workers still own the context"
    );
    drop(idle);
    assert!(weak.upgrade().is_none(), "retirement joins context owners");
}

#[test]
fn active_drop_waits_for_execution_before_releasing_state() {
    let (started_tx, started_rx) = bounded(1);
    let (release_tx, release_rx) = bounded(1);
    let (dropping_tx, dropping_rx) = bounded(1);
    let (dropped_tx, dropped_rx) = bounded(1);
    let mut context = context(0);
    context.first_task_gate = Some((started_tx, release_rx));
    let state = context.state.clone();
    let session =
        ReplaySession::<Handler>::new(20, context, Arc::new(Mutex::new(HashSet::new())), 1);
    session
        .schedule_execution(transaction(&Keypair::new(), 0), 0)
        .unwrap();
    started_rx.recv_timeout(WAIT).unwrap();
    let join = thread::spawn(move || {
        dropping_tx.send(()).unwrap();
        drop(session);
        dropped_tx.send(()).unwrap();
    });
    dropping_rx.recv_timeout(WAIT).unwrap();
    assert!(dropped_rx.recv_timeout(Duration::from_millis(50)).is_err());
    release_tx.send(()).unwrap();
    dropped_rx.recv_timeout(WAIT).unwrap();
    join.join().unwrap();
    assert_eq!(*state.commits.lock().unwrap(), [(0, 0)]);
}

#[derive(Clone, Debug)]
struct PanicServices {
    started: Sender<()>,
    release_panic: Receiver<()>,
    release_slow: Receiver<()>,
    slow_finished: Sender<()>,
}

#[derive(Debug)]
struct PanicHandler;

impl ReplaySessionHandler for PanicHandler {
    type Context = Context;
    type Services = PanicServices;

    fn handle(
        _result: &mut TransactionResult<()>,
        _timings: &mut ExecuteTimings,
        _context: &Context,
        _task: &Task,
        services: &PanicServices,
    ) {
        services.started.send(()).unwrap();
        if thread::current().name().unwrap().ends_with("00") {
            services.release_panic.recv_timeout(WAIT).unwrap();
            panic!("controlled first-handler panic");
        }
        services.release_slow.recv_timeout(WAIT).unwrap();
        services.slow_finished.send(()).unwrap();
    }
}

/// Characterize the unchanged native failure path, not an acceptance test for
/// BM restoration: a caught native panic is insufficient proof of quiescence.
#[test]
fn native_panic_propagation_is_not_a_quiescence_barrier() {
    let (started_tx, started_rx) = bounded(2);
    let (panic_tx, panic_rx) = bounded(1);
    let (slow_tx, slow_rx) = bounded(1);
    let (slow_finished_tx, slow_finished_rx) = bounded(1);
    let (finished_tx, finished_rx) = bounded(1);
    let session = ReplaySession::<PanicHandler>::new(
        21,
        context(0),
        PanicServices {
            started: started_tx,
            release_panic: panic_rx,
            release_slow: slow_rx,
            slow_finished: slow_finished_tx,
        },
        2,
    );
    // Independent accounts ensure both handlers enter execution before either
    // is released. The first native join handle belongs to handler 00.
    for id in 0..2 {
        session
            .schedule_execution(transaction(&Keypair::new(), id), id as u128)
            .unwrap();
    }
    started_rx.recv_timeout(WAIT).unwrap();
    started_rx.recv_timeout(WAIT).unwrap();
    let join = thread::spawn(move || {
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            // Native completion can race panic detection and return an idle
            // candidate first. Include its retirement inside the catch boundary.
            drop(session.finish());
        }));
        finished_tx.send(result.is_err()).unwrap();
    });
    panic_tx.send(()).unwrap();
    let propagated = finished_rx.recv_timeout(WAIT);
    let still_executing = slow_finished_rx.try_recv().is_err();
    // Always release the other worker before asserting the diagnostic result.
    slow_tx.send(()).unwrap();
    slow_finished_rx.recv_timeout(WAIT).unwrap();
    join.join().unwrap();
    assert!(propagated.unwrap());
    assert!(still_executing);
}
