#![cfg(feature = "agave-unstable-api")]

use {
    solana_instruction::{AccountMeta, Instruction},
    solana_message::{Message, SimpleAddressLoader},
    solana_pubkey::Pubkey,
    solana_runtime_transaction::runtime_transaction::RuntimeTransaction,
    solana_transaction::{
        Transaction,
        sanitized::{MessageHash, SanitizedTransaction},
    },
    solana_unified_scheduler_logic::{SchedulingStateMachine, Task, UsageQueue},
    std::{collections::HashMap, thread},
    test_case::test_matrix,
};

type ReplayTransaction = RuntimeTransaction<SanitizedTransaction>;

#[test_matrix([0, 1, 128])]
fn fifo_capacity_preserves_scheduling(initial_capacity: usize) {
    thread::spawn(move || {
        // SAFETY: Initialize exactly once on this fresh thread. All tasks, queues and
        // scheduler operations stay on this thread; no other scheduler accesses them.
        let mut scheduler = unsafe {
            SchedulingStateMachine::exclusively_initialize_current_thread_for_scheduling(None, None)
        };
        let address = Pubkey::new_unique();
        let mut queues = HashMap::new();
        let mut create_task = |id, writable| -> Task {
            let instruction = Instruction {
                program_id: Pubkey::default(),
                accounts: vec![if writable {
                    AccountMeta::new(address, false)
                } else {
                    AccountMeta::new_readonly(address, false)
                }],
                data: vec![],
            };
            let transaction = ReplayTransaction::try_create(
                Transaction::new_unsigned(Message::new(
                    &[instruction],
                    Some(&Pubkey::new_unique()),
                ))
                .into(),
                MessageHash::Compute,
                None,
                SimpleAddressLoader::Disabled,
                &Default::default(),
                true,
            )
            .unwrap();
            transaction.get_account_locks(64).unwrap();
            SchedulingStateMachine::create_task(transaction, id, &mut |key| {
                queues
                    .entry(key)
                    .or_insert_with(|| UsageQueue::new_fifo(initial_capacity))
                    .clone()
            })
        };

        // Descending IDs distinguish FIFO arrival order from priority ordering.
        let reader1 = scheduler
            .schedule_or_buffer_task(create_task(90, false), false)
            .unwrap();
        let reader2 = scheduler
            .schedule_or_buffer_task(create_task(80, false), false)
            .unwrap();
        for (id, writable) in [(70, true), (60, false), (50, false), (40, true)] {
            assert!(
                scheduler
                    .schedule_or_buffer_task(create_task(id, writable), false)
                    .is_none()
            );
        }
        scheduler.deschedule_task(&reader1);
        assert!(scheduler.schedule_next_unblocked_task().is_none());
        scheduler.deschedule_task(&reader2);
        let writer = scheduler.schedule_next_unblocked_task().unwrap();
        assert_eq!(writer.task_id(), 70);
        assert!(scheduler.schedule_next_unblocked_task().is_none());
        scheduler.deschedule_task(&writer);
        let reader3 = scheduler.schedule_next_unblocked_task().unwrap();
        let reader4 = scheduler.schedule_next_unblocked_task().unwrap();
        assert_eq!((reader3.task_id(), reader4.task_id()), (60, 50));
        assert!(scheduler.schedule_next_unblocked_task().is_none());
        scheduler.deschedule_task(&reader4);
        assert!(scheduler.schedule_next_unblocked_task().is_none());
        scheduler.deschedule_task(&reader3);
        let writer = scheduler.schedule_next_unblocked_task().unwrap();
        assert_eq!(writer.task_id(), 40);
        scheduler.deschedule_task(&writer);
        assert!(scheduler.has_no_active_task());
        scheduler.reinitialize();

        scheduler.buffer_task(create_task(30, true));
        scheduler.buffer_task(create_task(20, false));
        scheduler.buffer_task(create_task(10, true));
        assert!(scheduler.has_no_running_task());
        assert_eq!(scheduler.unblocked_task_queue_count(), 1);
        assert_eq!(scheduler.clear_and_reinitialize(), 3);
        assert_eq!(scheduler.total_task_count(), 0);
        assert!(scheduler.has_no_active_task());
        assert!(!scheduler.has_unblocked_task());

        // Reuse the same address cache after clearing all buffered and blocked work.
        let task = scheduler
            .schedule_or_buffer_task(create_task(0, true), false)
            .unwrap();
        scheduler.deschedule_task(&task);
        assert!(scheduler.has_no_active_task());
        assert_eq!(scheduler.clear_and_reinitialize(), 0);
    })
    .join()
    .unwrap();
}
