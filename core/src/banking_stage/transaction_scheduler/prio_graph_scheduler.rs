use {
    super::{
        in_flight_tracker::InFlightTracker,
        scheduler_error::SchedulerError,
        thread_aware_account_locks::{ThreadAwareAccountLocks, ThreadId, ThreadSet},
        transaction_state::SanitizedTransactionTTL,
        transaction_state_container::TransactionStateContainer,
    },
    crate::banking_stage::{
        consumer::TARGET_NUM_TRANSACTIONS_PER_BATCH,
        read_write_account_set::ReadWriteAccountSet,
        scheduler_messages::{ConsumeWork, FinishedConsumeWork, TransactionBatchId, TransactionId},
        transaction_scheduler::transaction_priority_id::TransactionPriorityId,
    },
    crossbeam_channel::{Receiver, Sender, TryRecvError},
    itertools::izip,
    prio_graph::{AccessKind, PrioGraph},
    solana_sdk::{pubkey::Pubkey, slot_history::Slot, transaction::SanitizedTransaction},
    std::collections::HashMap,
};

pub(crate) struct PrioGraphScheduler {
    in_flight_tracker: InFlightTracker,
    account_locks: ThreadAwareAccountLocks,
    consume_work_senders: Vec<Sender<ConsumeWork>>,
    finished_consume_work_receiver: Receiver<FinishedConsumeWork>,
    look_ahead_window_size: usize,
}

impl PrioGraphScheduler {
    pub(crate) fn new(
        consume_work_senders: Vec<Sender<ConsumeWork>>,
        finished_consume_work_receiver: Receiver<FinishedConsumeWork>,
    ) -> Self {
        let num_threads = consume_work_senders.len();
        Self {
            in_flight_tracker: InFlightTracker::new(num_threads),
            account_locks: ThreadAwareAccountLocks::new(num_threads),
            consume_work_senders,
            finished_consume_work_receiver,
            look_ahead_window_size: 2048,
        }
    }

    /// Schedule transactions from the given `TransactionStateContainer` to be consumed by the
    /// worker threads. Returns the number of transactions scheduled, or an error.
    ///
    /// Uses a `PrioGraph` to perform look-ahead during the scheduling of transactions.
    /// This, combined with internal tracking of threads' in-flight transactions, allows
    /// for load-balancing while prioritizing scheduling transactions onto threads that will
    /// not cause conflicts in the near future.
    pub(crate) fn schedule<const BLOCKING_MODE: bool>(
        &mut self,
        container: &mut TransactionStateContainer,
    ) -> Result<usize, SchedulerError> {
        let num_threads = self.consume_work_senders.len();
        let mut batches = Batches::new(num_threads);
        let mut chain_id_to_thread_index = HashMap::new();
        // Some transactions may be unschedulable due to multi-thread conflicts.
        // These transactions cannot be scheduled until some conflicting work is completed.
        // However, the scheduler should not allow other transactions that conflict with
        // these transactions to be scheduled before them.
        let mut unschedulable_ids = Vec::new();
        let mut blocking_locks = ReadWriteAccountSet::default();
        let mut prio_graph = PrioGraph::new(|id: &TransactionPriorityId, _graph_node| *id);

        // Create the initial look-ahead window.
        let mut num_pushed = 0;
        for _ in 0..self.look_ahead_window_size {
            let Some(id) = container.pop() else {
                break;
            };

            let transaction = container.get_transaction_ttl(&id.id).unwrap();
            prio_graph.insert_transaction(id, Self::get_transaction_resource_access(transaction));
            num_pushed += 1;
        }

        let mut unblock_this_batch =
            Vec::with_capacity(self.consume_work_senders.len() * TARGET_NUM_TRANSACTIONS_PER_BATCH);
        const MAX_TRANSACTIONS_PER_SCHEDULING_PASS: usize = 100_000;
        let mut num_scheduled = 0;
        let mut sent_scheduled_transactions = 0;
        while num_scheduled < MAX_TRANSACTIONS_PER_SCHEDULING_PASS {
            // If **not** in `BLOCKING_MODE`, we can check if we've scheduled everything here.
            // TODO: Expose interface for prio-graph: `is_empty_top()` vs `is_empty_graph()`
            if !BLOCKING_MODE {
                // If nothing is in the main-queue of the `PrioGraph` then there's nothing left to schedule.
                if prio_graph.is_empty() {
                    break;
                }
            }

            // Always break if we've scheduled everything that has been pushed into the prio-graph
            if num_scheduled == num_pushed {
                break;
            }

            while let Some(id) = prio_graph.pop() {
                unblock_this_batch.push(id);

                // Push next transaction from container into the `PrioGraph` look-ahead window.
                if let Some(next_id) = container.pop() {
                    let transaction = container.get_transaction_ttl(&next_id.id).unwrap();
                    prio_graph.insert_transaction(
                        next_id,
                        Self::get_transaction_resource_access(transaction),
                    );
                    num_pushed += 1;
                }

                if num_scheduled >= MAX_TRANSACTIONS_PER_SCHEDULING_PASS {
                    break;
                }

                // Should always be in the container, but can just skip if it is not for some reason.
                let Some(transaction_state) = container.get_mut_transaction_state(&id.id) else {
                    continue;
                };

                let transaction = &transaction_state.transaction_ttl().transaction;

                // Check if this transaction conflicts with any blocked transactions
                if !blocking_locks.check_locks(transaction.message()) {
                    blocking_locks.take_locks(transaction.message());
                    unschedulable_ids.push(id);
                    continue;
                }

                let maybe_chain_thread = chain_id_to_thread_index
                    .get(&prio_graph.chain_id(&id))
                    .copied();

                // Schedule the transaction if it can be.
                let transaction_locks = transaction.get_account_locks_unchecked();
                let Some(thread_id) = self.account_locks.try_lock_accounts(
                    transaction_locks.writable.into_iter(),
                    transaction_locks.readonly.into_iter(),
                    ThreadSet::any(num_threads),
                    |thread_set| {
                        Self::select_thread(
                            thread_set,
                            maybe_chain_thread,
                            &batches.transactions,
                            self.in_flight_tracker.num_in_flight_per_thread(),
                        )
                    },
                ) else {
                    blocking_locks.take_locks(transaction.message());
                    unschedulable_ids.push(id);
                    continue;
                };

                // Track total number of scheduled transactions
                num_scheduled += 1;

                // Track the chain-id to thread-index mapping.
                chain_id_to_thread_index.insert(prio_graph.chain_id(&id), thread_id);

                let sanitized_transaction_ttl = transaction_state.transition_to_pending();
                let cu_limit = transaction_state
                    .transaction_priority_details()
                    .compute_unit_limit;

                let SanitizedTransactionTTL {
                    transaction,
                    max_age_slot,
                } = sanitized_transaction_ttl;

                batches.transactions[thread_id].push(transaction);
                batches.ids[thread_id].push(id.id);
                batches.max_age_slots[thread_id].push(max_age_slot);
                batches.total_cus[thread_id] += cu_limit;

                // If target batch size is reached, send only this batch.
                if batches.ids[thread_id].len() >= TARGET_NUM_TRANSACTIONS_PER_BATCH {
                    sent_scheduled_transactions += self.send_batch(&mut batches, thread_id)?;
                }
            }

            // Send all non-empty batches
            sent_scheduled_transactions += self.send_batches(&mut batches)?;

            if num_scheduled == num_pushed {
                break;
            }

            if BLOCKING_MODE {
                // If blocking mode we will wait for at least one batch to be completed before
                // moving forward.
                let mut received_finished = false;
                while !received_finished {
                    loop {
                        if self.try_receive_completed(
                            container,
                            Some(|ids: &[TransactionPriorityId]| {
                                ids.into_iter().for_each(|id| prio_graph.unblock(id));
                            }),
                        )? {
                            received_finished = true;
                        } else if received_finished {
                            break;
                        }
                    }
                }
            } else {
                // Unblock all transactions that were blocked by the transactions that were just sent.
                for id in unblock_this_batch.drain(..) {
                    prio_graph.unblock(&id);
                }
            }
        }

        // Send batches for any remaining transactions
        sent_scheduled_transactions += self.send_batches(&mut batches)?;

        // Push unschedulable ids back into the container
        for id in unschedulable_ids {
            container.push_id_into_queue(id);
        }

        // Push remaining transactions back into the container
        while let Some(id) = prio_graph.pop_and_unblock() {
            container.push_id_into_queue(id);
        }

        assert!(
            num_scheduled == sent_scheduled_transactions,
            "count of sent and scheduled should be consistent at end"
        );

        Ok(num_scheduled)
    }

    pub fn receive_completed(
        &mut self,
        container: &mut TransactionStateContainer,
    ) -> Result<(), SchedulerError> {
        while self.try_receive_completed(container, Some(|_: &_| {}))? {}
        Ok(())
    }

    /// Receive completed batches of transactions.
    fn try_receive_completed<F: FnOnce(&[TransactionPriorityId])>(
        &mut self,
        container: &mut TransactionStateContainer,
        handler: Option<F>,
    ) -> Result<bool, SchedulerError> {
        match self.finished_consume_work_receiver.try_recv() {
            Ok(FinishedConsumeWork {
                work:
                    ConsumeWork {
                        batch_id,
                        ids,
                        transactions,
                        max_age_slots,
                    },
                retryable_indexes,
            }) => {
                // Free the locks
                self.complete_batch(batch_id, &transactions);

                // Call handler
                if let Some(handler) = handler {
                    let priority_ids: Vec<_> = ids
                        .iter()
                        .map(|id| TransactionPriorityId {
                            priority: container.get_mut_transaction_state(id).unwrap().priority(),
                            id: *id,
                        })
                        .collect();
                    handler(&priority_ids)
                }

                // Retryable transactions should be inserted back into the container
                // TODO: if actively scheduling, might consider inserting to prio-graph as well
                let mut retryable_iter = retryable_indexes.into_iter().peekable();
                for (index, (id, transaction, max_age_slot)) in
                    izip!(ids, transactions, max_age_slots).enumerate()
                {
                    if let Some(retryable_index) = retryable_iter.peek() {
                        if *retryable_index == index {
                            container.retry_transaction(
                                id,
                                SanitizedTransactionTTL {
                                    transaction,
                                    max_age_slot,
                                },
                            );
                            retryable_iter.next();
                            continue;
                        }
                    }
                    container.remove_by_id(&id);
                }

                Ok(true)
            }
            Err(TryRecvError::Empty) => Ok(false),
            Err(TryRecvError::Disconnected) => Err(SchedulerError::DisconnectedRecvChannel(
                "finished consume work",
            )),
        }
    }

    /// Mark a given `TransactionBatchId` as completed.
    /// This will update the internal tracking, including account locks.
    fn complete_batch(
        &mut self,
        batch_id: TransactionBatchId,
        transactions: &[SanitizedTransaction],
    ) {
        let thread_id = self.in_flight_tracker.complete_batch(batch_id);
        for transaction in transactions {
            let account_locks = transaction.get_account_locks_unchecked();
            self.account_locks.unlock_accounts(
                account_locks.writable.into_iter(),
                account_locks.readonly.into_iter(),
                thread_id,
            );
        }
    }

    /// Send all batches of transactions to the worker threads.
    /// Returns the number of transactions sent.
    fn send_batches(&mut self, batches: &mut Batches) -> Result<usize, SchedulerError> {
        (0..self.consume_work_senders.len())
            .map(|thread_index| self.send_batch(batches, thread_index))
            .sum()
    }

    /// Send a batch of transactions to the given thread's `ConsumeWork` channel.
    /// Returns the number of transactions sent.
    fn send_batch(
        &mut self,
        batches: &mut Batches,
        thread_index: usize,
    ) -> Result<usize, SchedulerError> {
        if batches.ids[thread_index].is_empty() {
            return Ok(0);
        }

        let (ids, transactions, max_age_slots, total_cus) = batches.take_batch(thread_index);

        let batch_id = self
            .in_flight_tracker
            .track_batch(ids.len(), total_cus, thread_index);

        let num_scheduled = ids.len();
        let work = ConsumeWork {
            batch_id,
            ids,
            transactions,
            max_age_slots,
        };
        self.consume_work_senders[thread_index]
            .send(work)
            .map_err(|_| SchedulerError::DisconnectedSendChannel("consume work sender"))?;

        Ok(num_scheduled)
    }

    /// Given the schedulable `thread_set`, select the thread with the least amount
    /// of work queued up.
    /// Currently, "work" is just defined as the number of transactions.
    ///
    /// If the `chain_thread` is available, this thread will be selected, regardless of
    /// load-balancing.
    ///
    /// Panics if the `thread_set` is empty.
    fn select_thread(
        thread_set: ThreadSet,
        chain_thread: Option<ThreadId>,
        batches_per_thread: &[Vec<SanitizedTransaction>],
        in_flight_per_thread: &[usize],
    ) -> ThreadId {
        if let Some(chain_thread) = chain_thread {
            if thread_set.contains(chain_thread) {
                return chain_thread;
            }
        }

        thread_set
            .contained_threads_iter()
            .map(|thread_id| {
                (
                    thread_id,
                    batches_per_thread[thread_id].len() + in_flight_per_thread[thread_id],
                )
            })
            .min_by(|a, b| a.1.cmp(&b.1))
            .map(|(thread_id, _)| thread_id)
            .unwrap()
    }

    /// Gets accessed resources for use in `PrioGraph`.
    fn get_transaction_resource_access(
        transaction: &SanitizedTransactionTTL,
    ) -> impl Iterator<Item = (Pubkey, AccessKind)> + '_ {
        let message = transaction.transaction.message();
        message
            .account_keys()
            .iter()
            .enumerate()
            .map(|(index, key)| {
                if message.is_writable(index) {
                    (*key, AccessKind::Write)
                } else {
                    (*key, AccessKind::Read)
                }
            })
    }
}

struct Batches {
    ids: Vec<Vec<TransactionId>>,
    transactions: Vec<Vec<SanitizedTransaction>>,
    max_age_slots: Vec<Vec<Slot>>,
    total_cus: Vec<u64>,
}

impl Batches {
    fn new(num_threads: usize) -> Self {
        Self {
            ids: vec![Vec::with_capacity(TARGET_NUM_TRANSACTIONS_PER_BATCH); num_threads],
            transactions: vec![Vec::with_capacity(TARGET_NUM_TRANSACTIONS_PER_BATCH); num_threads],
            max_age_slots: vec![Vec::with_capacity(TARGET_NUM_TRANSACTIONS_PER_BATCH); num_threads],
            total_cus: vec![0; num_threads],
        }
    }

    fn take_batch(
        &mut self,
        thread_id: ThreadId,
    ) -> (
        Vec<TransactionId>,
        Vec<SanitizedTransaction>,
        Vec<Slot>,
        u64,
    ) {
        (
            core::mem::replace(
                &mut self.ids[thread_id],
                Vec::with_capacity(TARGET_NUM_TRANSACTIONS_PER_BATCH),
            ),
            core::mem::replace(
                &mut self.transactions[thread_id],
                Vec::with_capacity(TARGET_NUM_TRANSACTIONS_PER_BATCH),
            ),
            core::mem::replace(
                &mut self.max_age_slots[thread_id],
                Vec::with_capacity(TARGET_NUM_TRANSACTIONS_PER_BATCH),
            ),
            core::mem::replace(&mut self.total_cus[thread_id], 0),
        )
    }
}

#[cfg(test)]
mod tests {
    use {
        super::*,
        crate::banking_stage::consumer::TARGET_NUM_TRANSACTIONS_PER_BATCH,
        crossbeam_channel::{unbounded, Receiver},
        itertools::Itertools,
        solana_runtime::transaction_priority_details::TransactionPriorityDetails,
        solana_sdk::{
            compute_budget::ComputeBudgetInstruction, hash::Hash, message::Message, pubkey::Pubkey,
            signature::Keypair, signer::Signer, system_instruction, transaction::Transaction,
        },
        std::{borrow::Borrow, time::Duration},
    };

    macro_rules! txid {
        ($value:expr) => {
            TransactionId::new($value)
        };
    }

    macro_rules! txids {
        ([$($element:expr),*]) => {
            vec![ $(txid!($element)),* ]
        };
    }

    fn create_test_frame(
        num_threads: usize,
    ) -> (
        PrioGraphScheduler,
        Vec<Receiver<ConsumeWork>>,
        Sender<FinishedConsumeWork>,
    ) {
        let (consume_work_senders, consume_work_receivers) =
            (0..num_threads).map(|_| unbounded()).unzip();
        let (finished_consume_work_sender, finished_consume_work_receiver) = unbounded();
        let scheduler =
            PrioGraphScheduler::new(consume_work_senders, finished_consume_work_receiver);
        (
            scheduler,
            consume_work_receivers,
            finished_consume_work_sender,
        )
    }

    fn prioritized_tranfers(
        from_keypair: &Keypair,
        to_pubkeys: impl IntoIterator<Item = impl Borrow<Pubkey>>,
        lamports: u64,
        priority: u64,
    ) -> SanitizedTransaction {
        let to_pubkeys_lamports = to_pubkeys
            .into_iter()
            .map(|pubkey| *pubkey.borrow())
            .zip(std::iter::repeat(lamports))
            .collect_vec();
        let mut ixs =
            system_instruction::transfer_many(&from_keypair.pubkey(), &to_pubkeys_lamports);
        let prioritization = ComputeBudgetInstruction::set_compute_unit_price(priority);
        ixs.push(prioritization);
        let message = Message::new(&ixs, Some(&from_keypair.pubkey()));
        let tx = Transaction::new(&[from_keypair], message, Hash::default());
        SanitizedTransaction::from_transaction_for_tests(tx)
    }

    fn create_container(
        tx_infos: impl IntoIterator<
            Item = (
                impl Borrow<Keypair>,
                impl IntoIterator<Item = impl Borrow<Pubkey>>,
                u64,
                u64,
            ),
        >,
    ) -> TransactionStateContainer {
        let mut container = TransactionStateContainer::with_capacity(10 * 1024);
        for (index, (from_keypair, to_pubkeys, lamports, priority)) in
            tx_infos.into_iter().enumerate()
        {
            let id = TransactionId::new(index as u64);
            let transaction =
                prioritized_tranfers(from_keypair.borrow(), to_pubkeys, lamports, priority);
            let transaction_ttl = SanitizedTransactionTTL {
                transaction,
                max_age_slot: Slot::MAX,
            };
            container.insert_new_transaction(
                id,
                transaction_ttl,
                TransactionPriorityDetails {
                    priority,
                    compute_unit_limit: 1,
                },
            );
        }

        container
    }

    fn collect_work(
        receiver: &Receiver<ConsumeWork>,
    ) -> (Vec<ConsumeWork>, Vec<Vec<TransactionId>>) {
        receiver
            .try_iter()
            .map(|work| {
                let ids = work.ids.clone();
                (work, ids)
            })
            .unzip()
    }

    #[test]
    fn test_schedule_disconnected_channel() {
        let (mut scheduler, work_receivers, _finshed_work_sender) = create_test_frame(1);
        let mut container = create_container([(&Keypair::new(), &[Pubkey::new_unique()], 1, 1)]);

        drop(work_receivers); // explicitly drop receivers
        assert_matches!(
            scheduler.schedule::<false>(&mut container),
            Err(SchedulerError::DisconnectedSendChannel(_))
        );
    }

    #[test]
    fn test_schedule_single_threaded_no_conflicts() {
        let (mut scheduler, work_receivers, _finshed_work_sender) = create_test_frame(1);
        let mut container = create_container([
            (&Keypair::new(), &[Pubkey::new_unique()], 1, 1),
            (&Keypair::new(), &[Pubkey::new_unique()], 2, 2),
        ]);

        let num_scheduled = scheduler.schedule::<false>(&mut container).unwrap();
        assert_eq!(num_scheduled, 2);
        assert_eq!(collect_work(&work_receivers[0]).1, vec![txids!([1, 0])]);
    }

    #[test]
    fn test_schedule_single_threaded_conflict() {
        let (mut scheduler, work_receivers, _finshed_work_sender) = create_test_frame(1);
        let pubkey = Pubkey::new_unique();
        let mut container = create_container([
            (&Keypair::new(), &[pubkey], 1, 1),
            (&Keypair::new(), &[pubkey], 1, 2),
        ]);

        let num_scheduled = scheduler.schedule::<false>(&mut container).unwrap();
        assert_eq!(num_scheduled, 2);
        assert_eq!(
            collect_work(&work_receivers[0]).1,
            vec![txids!([1]), txids!([0])]
        );
    }

    #[test]
    fn test_schedule_consume_single_threaded_multi_batch() {
        let (mut scheduler, work_receivers, _finshed_work_sender) = create_test_frame(1);
        let mut container = create_container(
            (0..4 * TARGET_NUM_TRANSACTIONS_PER_BATCH)
                .map(|i| (Keypair::new(), [Pubkey::new_unique()], i as u64, 1)),
        );

        // expect 4 full batches to be scheduled
        let num_scheduled = scheduler.schedule::<false>(&mut container).unwrap();
        assert_eq!(num_scheduled, 4 * TARGET_NUM_TRANSACTIONS_PER_BATCH);

        let thread0_work_counts: Vec<_> = work_receivers[0]
            .try_iter()
            .map(|work| work.ids.len())
            .collect();
        assert_eq!(thread0_work_counts, [TARGET_NUM_TRANSACTIONS_PER_BATCH; 4]);
    }

    #[test]
    fn test_schedule_simple_thread_selection() {
        let (mut scheduler, work_receivers, _finshed_work_sender) = create_test_frame(2);
        let mut container =
            create_container((0..4).map(|i| (Keypair::new(), [Pubkey::new_unique()], 1, i)));

        let num_scheduled = scheduler.schedule::<false>(&mut container).unwrap();
        assert_eq!(num_scheduled, 4);
        assert_eq!(collect_work(&work_receivers[0]).1, [txids!([3, 1])]);
        assert_eq!(collect_work(&work_receivers[1]).1, [txids!([2, 0])]);
    }

    #[test]
    fn test_schedule_look_ahead() {
        let (mut scheduler, work_receivers, _finshed_work_sender) = create_test_frame(2);

        let accounts = (0..4).map(|_| Keypair::new()).collect_vec();
        let mut container = create_container([
            (&accounts[0], &[accounts[1].pubkey()], 1, 2),
            (&accounts[2], &[accounts[3].pubkey()], 1, 1),
            (&accounts[1], &[accounts[2].pubkey()], 1, 0),
        ]);

        // high priority transactions [0, 1] do not conflict, and would be
        // scheduled to *different* threads without chain-id look-ahead.
        // Because low priority transaction [2] conflicts with both, it will
        // cause transaction [1] to be scheduled onto the same thread as
        // transaction [0].
        let num_scheduled = scheduler.schedule::<false>(&mut container).unwrap();
        assert_eq!(num_scheduled, 3);
        assert_eq!(
            collect_work(&work_receivers[0]).1,
            [txids!([0, 1]), txids!([2])]
        );
    }

    #[test]
    fn test_schedule_priority_guard() {
        let (mut scheduler, work_receivers, _finshed_work_sender) = create_test_frame(2);
        // intentionally shorten the look-ahead window to cause unschedulable conflicts
        scheduler.look_ahead_window_size = 2;

        let accounts = (0..8).map(|_| Keypair::new()).collect_vec();
        let mut container = create_container([
            (&accounts[0], &[accounts[1].pubkey()], 1, 6),
            (&accounts[2], &[accounts[3].pubkey()], 1, 5),
            (&accounts[4], &[accounts[5].pubkey()], 1, 4),
            (&accounts[6], &[accounts[7].pubkey()], 1, 3),
            (&accounts[1], &[accounts[2].pubkey()], 1, 2),
            (&accounts[2], &[accounts[3].pubkey()], 1, 1),
        ]);

        // high priority transactions [0, 1, 2, 3] do not conflict, and are
        // scheduled onto threads in a round-robin fashion.
        // The look-ahead window is intentionally shortened, which leads to
        // transaction [4] being unschedulable due to conflicts with [0] and [1],
        // which were scheduled to different threads.
        // Transaction [5] is technically schedulable, but will not be scheduled
        // because it conflicts with [4].
        let num_scheduled = scheduler.schedule::<false>(&mut container).unwrap();
        assert_eq!(num_scheduled, 4);
        let (thread_0_work, thread_0_ids) = collect_work(&work_receivers[0]);
        assert_eq!(thread_0_ids, [txids!([0, 2])]);
        assert_eq!(collect_work(&work_receivers[1]).1, [txids!([1, 3])]);

        // Cannot schedule even on next pass because of lock conflicts
        let num_scheduled = scheduler.schedule::<false>(&mut container).unwrap();
        assert_eq!(num_scheduled, 0);

        // Complete batch on thread 0. Remaining txs can be scheduled onto thread 1
        scheduler.complete_batch(thread_0_work[0].batch_id, &thread_0_work[0].transactions);
        let num_scheduled = scheduler.schedule::<false>(&mut container).unwrap();
        assert_eq!(num_scheduled, 2);

        assert_eq!(
            collect_work(&work_receivers[1]).1,
            [txids!([4]), txids!([5])]
        );
    }

    #[test]
    fn test_blocking_mode() {
        let (mut scheduler, work_receivers, finished_work_sender) = create_test_frame(2);
        // intentionally shorten the look-ahead window to cause unschedulable conflicts
        scheduler.look_ahead_window_size = 2;

        let accounts = (0..8).map(|_| Keypair::new()).collect_vec();
        let mut container = create_container([
            (&accounts[0], &[accounts[1].pubkey()], 1, 6),
            (&accounts[2], &[accounts[3].pubkey()], 1, 5),
            (&accounts[4], &[accounts[5].pubkey()], 1, 4),
            (&accounts[6], &[accounts[7].pubkey()], 1, 3),
            (&accounts[1], &[accounts[2].pubkey()], 1, 2),
            (&accounts[2], &[accounts[3].pubkey()], 1, 1),
        ]);

        // To test blocking mode, we must spawn a separate thread for scheduling.
        let scheduling_thread = std::thread::spawn(move || {
            assert_eq!(scheduler.schedule::<true>(&mut container), Ok(6));
        });

        // high priority transactions [0, 1, 2, 3] do not conflict, and are
        // scheduled onto threads in a round-robin fashion.
        // The look-ahead window is intentionally shortened, which leads to
        // transaction [4] being unschedulable due to conflicts with [0] and [1],
        // which were scheduled to different threads.
        // Transaction [5] is technically schedulable, but will not be scheduled
        // because it conflicts with [4].

        // Collect initial work sent to threads 0 and 1:
        std::thread::sleep(Duration::from_millis(1000));
        let thread_0_works = collect_work(&work_receivers[0]);
        let thread_1_works = collect_work(&work_receivers[1]);
        assert_eq!(thread_0_works.1, [txids!([0, 2])]);
        assert_eq!(thread_1_works.1, [txids!([1, 3])]);

        // Scheduling thread will block until both of these are completed by sending back over the channel.
        finished_work_sender
            .send(FinishedConsumeWork {
                work: thread_0_works.0.into_iter().next().unwrap(),
                retryable_indexes: vec![],
            })
            .unwrap();
        finished_work_sender
            .send(FinishedConsumeWork {
                work: thread_1_works.0.into_iter().next().unwrap(),
                retryable_indexes: vec![],
            })
            .unwrap();

        // Now [4] can be scheduled, and should be sent a single work message.
        let thread_0_work = work_receivers[0].recv().unwrap();
        assert_eq!(thread_0_work.ids, txids!([4]));
        assert!(work_receivers[0].is_empty());

        // Scheduling thread will block until this is complete.
        finished_work_sender
            .send(FinishedConsumeWork {
                work: thread_0_work,
                retryable_indexes: vec![],
            })
            .unwrap();

        // Now [5] can be scheduled, and should be sent a single work message.
        // Since thread 1 still has in-flight work it should be sent to thread 0.
        let thread_0_work = work_receivers[0].recv().unwrap();
        assert_eq!(thread_0_work.ids, txids!([5]));
        assert!(work_receivers[0].is_empty());

        scheduling_thread.join().unwrap();
    }
}
