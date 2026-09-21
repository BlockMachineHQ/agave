//! Storage-independent access to the production verification worker/session engine.
//!
//! Execution state is opaque to scheduling. The handler must finish execution,
//! commit and post-commit checks before returning; the production engine owns
//! priority, completion/descheduling, error accumulation and context transitions.
//! This experimental interface does not provide a memory bound or a Bank
//! lifecycle attachment. It preserves native panic behavior: a session result
//! can race handler-panic termination, and catching a later join panic is not
//! a worker-quiescence barrier. Do not use it for recoverable state restoration.

use {
    crate::{ThreadManager, UsageQueueLoader},
    solana_clock::Slot,
    solana_runtime::installed_scheduler_pool::{
        ResultWithTimings, ScheduleResult, SchedulerId, initialized_result_with_timings,
    },
    solana_runtime_transaction::runtime_transaction::RuntimeTransaction,
    solana_svm_timings::ExecuteTimings,
    solana_transaction::sanitized::SanitizedTransaction,
    solana_transaction_error::TransactionResult,
    solana_unified_scheduler_logic::{OrderedTaskId, Task},
    std::fmt::Debug,
};

/// Owned execution state. `slot` is diagnostic, not an identity check: adapters
/// must bind exact bank/attempt identity and prevent state replacement until the
/// session has completed. Contexts can remain retained by idle worker threads.
pub trait ReplaySessionContext: Clone + Debug + Send + Sync + 'static {
    fn slot(&self) -> Slot;
}

pub trait ReplaySessionHandler: Debug + Send + Sync + 'static {
    type Context: ReplaySessionContext;
    type Services: Clone + Debug + Send + Sync + 'static;

    fn handle(
        result: &mut TransactionResult<()>,
        timings: &mut ExecuteTimings,
        context: &Self::Context,
        task: &Task,
        services: &Self::Services,
    );
}

/// An active verification session on a dedicated native scheduler and handlers.
/// Owns task construction so usage queues cannot cross scheduler/token domains.
#[derive(Debug)]
pub struct ReplaySession<H: ReplaySessionHandler> {
    workers: Option<IdleReplaySession<H>>,
}

/// Workers returned by native session completion and their queue cache. Resume
/// consumes this handle; transaction-error sessions are retired instead. A
/// native handler panic can race completion, so this handle is not evidence that
/// no fatal worker fault occurred. Dropping retires the native workers.
/// Completion does not release the previous context held by idle handlers.
#[derive(Debug)]
pub struct IdleReplaySession<H: ReplaySessionHandler> {
    manager: ThreadManager<H>,
    queues: UsageQueueLoader,
}

impl<H: ReplaySessionHandler> ReplaySession<H> {
    pub fn new(
        id: SchedulerId,
        context: H::Context,
        services: H::Services,
        handler_count: usize,
    ) -> Self {
        let mut manager = ThreadManager::new(id);
        manager.start_threads(
            context,
            initialized_result_with_timings(),
            services,
            handler_count,
        );
        Self {
            workers: Some(IdleReplaySession {
                manager,
                queues: UsageQueueLoader::new_verification(),
            }),
        }
    }

    pub fn schedule_execution(
        &self,
        transaction: RuntimeTransaction<SanitizedTransaction>,
        task_id: OrderedTaskId,
    ) -> ScheduleResult {
        let workers = self.workers.as_ref().unwrap();
        let task = workers.queues.create_task(transaction, task_id);
        workers.manager.send_task(task)
    }

    /// Uses the production end-session path, including abort joins. A successful
    /// result follows native reuse rules; a transaction error retires the workers.
    /// Handler panics propagate when the production path joins workers, which
    /// can be later than this call if a panic races successful-result delivery.
    pub fn finish(mut self) -> (ResultWithTimings, Option<IdleReplaySession<H>>) {
        let mut workers = self.workers.take().unwrap();
        workers.manager.end_session();
        let result = workers.manager.take_session_result_with_timings();
        let reusable = !workers.manager.are_threads_joined();
        (result, reusable.then_some(workers))
    }
}

impl<H: ReplaySessionHandler> Drop for ReplaySession<H> {
    fn drop(&mut self) {
        // Preserve native unwinding behavior; do not claim a quiescence barrier
        // when the production engine itself skips joins during unwinding.
        if !std::thread::panicking()
            && let Some(workers) = &mut self.workers
        {
            workers.manager.end_session();
            let _ = workers.manager.take_session_result_with_timings();
        }
    }
}

impl<H: ReplaySessionHandler> IdleReplaySession<H> {
    pub fn id(&self) -> SchedulerId {
        self.manager.scheduler_id
    }

    pub fn is_overgrown(&self, max_usage_queue_count: usize) -> bool {
        self.queues.is_overgrown(max_usage_queue_count)
    }

    /// A fresh session; cumulative same-bank resume is deliberately not exposed
    /// by this initial facade. The production Bank attachment retains its native
    /// cumulative-result and pause behavior on the same generic engine.
    pub fn resume(mut self, context: H::Context) -> ReplaySession<H> {
        self.manager
            .start_session(context, initialized_result_with_timings());
        ReplaySession {
            workers: Some(self),
        }
    }
}
