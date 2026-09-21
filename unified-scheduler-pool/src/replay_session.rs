//! Storage-independent access to the production verification worker/session engine.
//!
//! Execution state is opaque to scheduling. The handler must finish execution,
//! commit and post-commit checks before returning; the production engine owns
//! priority, completion/descheduling, error accumulation and context transitions.
//! This experimental interface does not provide a memory bound or a Bank
//! lifecycle attachment. Fatal worker failures propagate only after the shared
//! engine joins every worker; they never authorize session reuse.

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

/// A non-protocol execution-service failure. The adapter retains its concrete
/// error/evidence; this signal aborts and retires a session without invoking the
/// process panic hook or inventing a TransactionError.
#[derive(Debug)]
pub struct ExecutionFault;

pub trait ReplaySessionHandler: Debug + Send + Sync + 'static {
    type Context: ReplaySessionContext;
    type Services: Clone + Debug + Send + Sync + 'static;

    fn handle(
        result: &mut TransactionResult<()>,
        timings: &mut ExecuteTimings,
        context: &Self::Context,
        task: &Task,
        services: &Self::Services,
    ) -> Result<(), ExecutionFault>;

    /// Observational notification after result delivery, including fatal handler
    /// notification. Implementations must not panic or access mutable bank state.
    fn task_delivered(_context: &Self::Context, _task_id: OrderedTaskId) {}
}

/// An active verification session on a dedicated native scheduler and handlers.
/// Owns task construction so usage queues cannot cross scheduler/token domains.
#[derive(Debug)]
pub struct ReplaySession<H: ReplaySessionHandler> {
    workers: Option<IdleReplaySession<H>>,
}

/// Workers returned by native session completion and their queue cache. Resume
/// consumes this handle; failed sessions are retired instead. Dropping retires
/// the native workers.
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
        let queues = UsageQueueLoader::new_verification();
        manager.start_threads(
            context,
            initialized_result_with_timings(),
            services,
            handler_count,
            queues.usage_queue_loader().usage_queues.clone(),
        );
        Self {
            workers: Some(IdleReplaySession { manager, queues }),
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

    pub fn recover_error_after_abort(
        &mut self,
    ) -> Result<solana_transaction_error::TransactionError, ExecutionFault> {
        let manager = &mut self.workers.as_mut().unwrap().manager;
        manager.ensure_join_threads(true);
        if manager.execution_fault {
            Err(ExecutionFault)
        } else {
            Ok(manager
                .session_result_with_timings
                .as_ref()
                .unwrap()
                .0
                .clone()
                .unwrap_err())
        }
    }

    pub fn pause_for_recent_blockhash(&mut self) {
        self.workers.as_mut().unwrap().manager.end_session();
    }

    /// Allocation setting only; must be selected before submitting any task.
    pub fn with_fifo_initial_capacity(mut self, capacity: usize) -> Self {
        let UsageQueueLoader::OwnedBySelf {
            usage_queue_loader_inner,
        } = &mut self.workers.as_mut().unwrap().queues;
        assert_eq!(usage_queue_loader_inner.count(), 0);
        usage_queue_loader_inner.fifo_initial_capacity = Some(capacity);
        self
    }

    /// Uses the production end-session path, including abort joins. A successful
    /// result follows native reuse rules; a transaction error retires the workers.
    /// Handler panics propagate after every worker has joined.
    pub fn finish(self) -> (ResultWithTimings, Option<IdleReplaySession<H>>) {
        self.finish_outcome().expect("execution backend failed")
    }

    /// Complete with a distinct, non-panicking infrastructure-failure outcome.
    pub fn finish_outcome(
        mut self,
    ) -> Result<(ResultWithTimings, Option<IdleReplaySession<H>>), ExecutionFault> {
        let mut workers = self.workers.take().unwrap();
        workers.manager.end_session();
        let result = workers.manager.take_session_result_with_timings();
        let reusable = !workers.manager.are_threads_joined();
        if workers.manager.execution_fault {
            assert!(!reusable);
            Err(ExecutionFault)
        } else {
            Ok((result, reusable.then_some(workers)))
        }
    }
}

impl<H: ReplaySessionHandler> Drop for ReplaySession<H> {
    fn drop(&mut self) {
        if let Some(workers) = &mut self.workers {
            let unwinding = std::thread::panicking();
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                if unwinding {
                    let _ = workers.manager.disconnect_new_task_sender();
                    workers.manager.ensure_join_threads(true);
                } else {
                    workers.manager.end_session();
                }
                let _ = workers.manager.take_session_result_with_timings();
            }));
            if let Err(panic) = result {
                if unwinding {
                    log::error!("replay worker failed while draining an unwinding session");
                } else {
                    std::panic::resume_unwind(panic);
                }
            }
        }
    }
}

impl<H: ReplaySessionHandler> IdleReplaySession<H> {
    /// Release per-address retained allocations at a completed-session boundary.
    /// Worker/token ownership stays unchanged; the next session recreates queues.
    pub fn clear_usage_queues(&mut self) {
        let UsageQueueLoader::OwnedBySelf {
            usage_queue_loader_inner,
        } = &mut self.queues;
        usage_queue_loader_inner.usage_queues.clear();
        usage_queue_loader_inner.usage_queues.shrink_to_fit();
    }
    pub fn id(&self) -> SchedulerId {
        self.manager.scheduler_id
    }

    pub fn is_overgrown(&self, max_usage_queue_count: usize) -> bool {
        self.queues.is_overgrown(max_usage_queue_count)
    }

    pub fn resume(self, context: H::Context) -> ReplaySession<H> {
        self.resume_with_result(context, initialized_result_with_timings())
    }

    pub fn resume_with_result(
        mut self,
        context: H::Context,
        result: ResultWithTimings,
    ) -> ReplaySession<H> {
        assert!(result.0.is_ok(), "cannot resume a failed bank");
        self.manager.start_session(context, result);
        ReplaySession {
            workers: Some(self),
        }
    }
}
