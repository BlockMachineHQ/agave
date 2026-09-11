#![cfg(feature = "agave-unstable-api")]

use {
    solana_instruction::{AccountMeta, Instruction},
    solana_message::Message,
    solana_pubkey::Pubkey,
    solana_runtime_transaction::runtime_transaction::RuntimeTransaction,
    solana_transaction::Transaction,
    solana_unified_scheduler_logic::{Capability, SchedulingStateMachine, Task, UsageQueue},
    std::{collections::HashMap, sync::Arc, thread},
    test_case::test_matrix,
};

fn with_scheduler(
    width: Option<usize>,
    active_cap: Option<usize>,
    run: impl FnOnce(&mut SchedulingStateMachine) + Send + 'static,
) {
    thread::spawn(move || {
        // SAFETY: Initialize once on this fresh thread; all scheduling and queue
        // mutation stays here, including across the repeated-session test.
        let mut scheduler = unsafe {
            SchedulingStateMachine::exclusively_initialize_current_thread_for_scheduling(
                width, active_cap,
            )
        };
        run(&mut scheduler);
        assert!(scheduler.has_no_active_task());
    })
    .join()
    .unwrap();
}

fn task_factory(capability: Capability) -> impl FnMut(u128, Pubkey, bool) -> Task {
    let mut queues = HashMap::new();
    move |id, address, writable| {
        let instruction = Instruction {
            program_id: Pubkey::default(),
            accounts: vec![if writable {
                AccountMeta::new(address, false)
            } else {
                AccountMeta::new_readonly(address, false)
            }],
            data: vec![],
        };
        let transaction = RuntimeTransaction::from_transaction_for_tests(
            Transaction::new_unsigned(Message::new(&[instruction], Some(&Pubkey::new_unique()))),
        );
        SchedulingStateMachine::create_task(transaction, id, &mut |key| {
            queues
                .entry(key)
                .or_insert_with(|| UsageQueue::new(&capability))
                .clone()
        })
    }
}

#[test]
fn saturated_width_does_not_pull_input_or_unblocked_work() {
    with_scheduler(Some(1), None, |scheduler| {
        let mut create = task_factory(Capability::FifoQueueing);
        let running = scheduler
            .schedule_next_task(|| Some(create(1, Pubkey::new_unique(), true)))
            .unwrap();
        scheduler.buffer_task(create(2, Pubkey::new_unique(), true));
        assert!(scheduler.has_unblocked_task());
        assert!(
            scheduler
                .schedule_next_task(|| panic!("input pulled while saturated"))
                .is_none()
        );
        assert_eq!(scheduler.total_task_count(), 2);
        scheduler.deschedule_task(&running);
        let unblocked = scheduler
            .schedule_next_task(|| panic!("input pulled ahead of unblocked work"))
            .unwrap();
        assert_eq!(unblocked.task_id(), 2);
        scheduler.deschedule_task(&unblocked);
        assert!(scheduler.schedule_next_task(|| None).is_none());
    });
    with_scheduler(Some(0), None, |scheduler| {
        assert!(
            scheduler
                .schedule_next_task(|| panic!("zero width consumed input"))
                .is_none()
        );
    });
}

#[test]
fn dependency_chain_precedes_lazy_independent_backlog() {
    with_scheduler(Some(2), None, |scheduler| {
        let mut create = task_factory(Capability::FifoQueueing);
        let hot = Pubkey::new_unique();
        let mut inputs = [
            (10, hot),
            (20, hot),
            (30, Pubkey::new_unique()),
            (40, Pubkey::new_unique()),
        ]
        .into_iter();
        let first = scheduler
            .schedule_next_task(|| inputs.next().map(|(id, key)| create(id, key, true)))
            .unwrap();
        assert_eq!(inputs.len(), 3);
        let independent = scheduler
            .schedule_next_task(|| inputs.next().map(|(id, key)| create(id, key, true)))
            .unwrap();
        assert_eq!((first.task_id(), independent.task_id()), (10, 30));
        assert_eq!(inputs.len(), 1);
        assert_eq!(scheduler.total_task_count(), 3);
        scheduler.deschedule_task(&first);
        let chain = scheduler
            .schedule_next_task(|| inputs.next().map(|(id, key)| create(id, key, true)))
            .unwrap();
        assert_eq!(chain.task_id(), 20);
        assert_eq!(inputs.len(), 1);
        scheduler.deschedule_task(&chain);
        let tail = scheduler
            .schedule_next_task(|| inputs.next().map(|(id, key)| create(id, key, true)))
            .unwrap();
        assert_eq!(tail.task_id(), 40);
        scheduler.deschedule_task(&tail);
        scheduler.deschedule_task(&independent);
    });
}

#[test]
fn fifo_readers_cannot_overtake_ledger_ordered_writers() {
    with_scheduler(Some(3), None, |scheduler| {
        let mut create = task_factory(Capability::FifoQueueing);
        let hot = Pubkey::new_unique();
        // Descending IDs distinguish FIFO admission from task-ID ordering.
        let mut inputs = [
            (90, false),
            (80, false),
            (70, true),
            (60, false),
            (50, false),
            (40, true),
        ]
        .into_iter();
        let mut next = || inputs.next().map(|(id, write)| create(id, hot, write));
        let reader1 = scheduler.schedule_next_task(&mut next).unwrap();
        let reader2 = scheduler.schedule_next_task(&mut next).unwrap();
        assert_eq!((reader1.task_id(), reader2.task_id()), (90, 80));
        // Spare width admits the entire blocked tail, but no later reader can run.
        assert!(scheduler.schedule_next_task(&mut next).is_none());
        assert_eq!(scheduler.total_task_count(), 6);
        scheduler.deschedule_task(&reader2);
        assert!(scheduler.schedule_next_task(|| None).is_none());
        scheduler.deschedule_task(&reader1);
        let writer = scheduler.schedule_next_task(|| None).unwrap();
        assert_eq!(writer.task_id(), 70);
        assert!(scheduler.schedule_next_task(|| None).is_none());
        scheduler.deschedule_task(&writer);
        let reader3 = scheduler.schedule_next_task(|| None).unwrap();
        let reader4 = scheduler.schedule_next_task(|| None).unwrap();
        assert_eq!((reader3.task_id(), reader4.task_id()), (60, 50));
        scheduler.deschedule_task(&reader3);
        assert!(scheduler.schedule_next_task(|| None).is_none());
        scheduler.deschedule_task(&reader4);
        let writer = scheduler.schedule_next_task(|| None).unwrap();
        assert_eq!(writer.task_id(), 40);
        scheduler.deschedule_task(&writer);
    });
}

#[test_matrix([Capability::FifoQueueing, Capability::PriorityQueueing])]
fn repeated_sessions_clear_blocked_ownership_and_reuse_queues(capability: Capability) {
    with_scheduler(Some(2), None, move |scheduler| {
        let mut create = task_factory(capability);
        let hot = Pubkey::new_unique();
        for _ in 0..3 {
            let running = scheduler
                .schedule_next_task(|| Some(create(1, hot, true)))
                .unwrap();
            let blocked = create(2, hot, true);
            let weak = Arc::downgrade(&blocked);
            let mut input = Some(blocked);
            assert!(scheduler.schedule_next_task(|| input.take()).is_none());
            assert!(weak.upgrade().is_some());
            // Consumer abort: execution finishes before native buffered cleanup.
            scheduler.deschedule_task(&running);
            drop(running);
            assert_eq!(scheduler.clear_and_reinitialize(), 1);
            assert!(weak.upgrade().is_none());
            assert_eq!(scheduler.total_task_count(), 0);
            assert!(!scheduler.has_unblocked_task());
            let next = scheduler
                .schedule_next_task(|| Some(create(3, hot, true)))
                .unwrap();
            scheduler.deschedule_task(&next);
            scheduler.reinitialize();
        }
    });
}

#[test_matrix([Capability::FifoQueueing, Capability::PriorityQueueing])]
fn active_cap_and_duplicate_dropping_are_preserved(capability: Capability) {
    with_scheduler(None, Some(2), move |scheduler| {
        let mut create = task_factory(capability);
        let hot = Pubkey::new_unique();
        let first = create(1, hot, true);
        let duplicate = first.clone();
        let mut input = Some(first);
        let running = scheduler.schedule_next_task(|| input.take()).unwrap();
        let blocked = create(2, hot, true);
        let excess = create(3, Pubkey::new_unique(), true);
        let blocked_weak = Arc::downgrade(&blocked);
        let excess_weak = Arc::downgrade(&excess);
        let mut inputs = [duplicate, blocked, excess].into_iter();
        assert!(scheduler.schedule_next_task(|| inputs.next()).is_none());
        assert_eq!(inputs.len(), 0);
        assert_eq!(scheduler.total_task_count(), 4);
        assert_eq!(scheduler.dropped_task_count(), 2);
        assert!(excess_weak.upgrade().is_none());
        assert!(blocked_weak.upgrade().is_some());
        scheduler.deschedule_task(&running);
        let blocked = scheduler.schedule_next_task(|| None).unwrap();
        assert_eq!(blocked.task_id(), 2);
        scheduler.deschedule_task(&blocked);
        scheduler.reinitialize();
        assert_eq!(scheduler.dropped_task_count(), 0);
    });
}

#[test]
fn zero_active_cap_consumes_and_drops_inputs_without_running_them() {
    with_scheduler(Some(1), Some(0), |scheduler| {
        let mut create = task_factory(Capability::FifoQueueing);
        let mut ids = 0..3;
        assert!(
            scheduler
                .schedule_next_task(|| ids.next().map(|id| create(id, Pubkey::new_unique(), true)))
                .is_none()
        );
        assert_eq!(scheduler.total_task_count(), 3);
        assert_eq!(scheduler.dropped_task_count(), 3);
    });
}
