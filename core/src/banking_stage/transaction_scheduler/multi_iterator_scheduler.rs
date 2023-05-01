use {
    super::{
        in_flight_tracker::InFlightTracker,
        sanitizer::Sanitizer,
        thread_aware_account_locks::{ThreadAwareAccountLocks, ThreadId, ThreadSet},
        transaction_id_generator::TransactionIdGenerator,
        transaction_packet_container::{SanitizedTransactionTTL, TransactionPacketContainer},
        transaction_priority_id::TransactionPriorityId,
        work_finisher::WorkFinisher,
    },
    crate::{
        banking_stage::{
            consumer::MAX_NUM_TRANSACTIONS_PER_BATCH,
            decision_maker::{BufferedPacketsDecision, DecisionMaker},
            scheduler_messages::{
                ConsumeWork, FinishedConsumeWork, FinishedForwardWork, ForwardWork,
                TransactionBatchId, TransactionId,
            },
        },
        banking_trace::BankingPacketReceiver,
        immutable_deserialized_packet::ImmutableDeserializedPacket,
        multi_iterator_scanner::{MultiIteratorScanner, ProcessingDecision},
        read_write_account_set::ReadWriteAccountSet,
    },
    crossbeam_channel::{Receiver, Sender},
    itertools::Itertools,
    solana_perf::perf_libs,
    solana_runtime::{
        bank::{Bank, BankStatusCache},
        bank_forks::BankForks,
        blockhash_queue::BlockhashQueue,
        transaction_error_metrics::TransactionErrorMetrics,
    },
    solana_sdk::{
        clock::{
            Slot, FORWARD_TRANSACTIONS_TO_LEADER_AT_SLOT_OFFSET, MAX_PROCESSING_AGE,
            MAX_TRANSACTION_FORWARDING_DELAY, MAX_TRANSACTION_FORWARDING_DELAY_GPU,
        },
        nonce::state::DurableNonce,
        transaction::SanitizedTransaction,
    },
    std::sync::{
        atomic::{AtomicBool, AtomicUsize, Ordering},
        Arc, RwLock, RwLockReadGuard,
    },
    thiserror::Error,
};

struct BatchIdGenerator {
    index: u64,
}

impl Default for BatchIdGenerator {
    fn default() -> Self {
        Self { index: u64::MAX }
    }
}

impl BatchIdGenerator {
    fn next(&mut self) -> TransactionBatchId {
        let index = self.index;
        self.index = self.index.wrapping_sub(1);
        TransactionBatchId::new(index)
    }
}

#[derive(Debug, Error)]
pub enum SchedulerError {
    #[error("Receiving channel disconnected: {0}")]
    DisconnectedReceiveChannel(&'static str),
    #[error("Sending channel disconnected: {0}")]
    DisconnectedSendChannel(&'static str),
}

pub struct MultiIteratorScheduler {
    /// Number of executing threads
    num_threads: usize,
    /// Limit on the number of in-flight transactions per thread
    thread_in_flight_limit: usize,
    /// Makes decision about whether to consume, forward, or do nothing with packets.
    decision_maker: DecisionMaker,
    /// Tracks locks for in-flight transactions
    account_locks: Arc<ThreadAwareAccountLocks>,
    /// Tracks all transactions/packets within scheduler
    container: Arc<TransactionPacketContainer>,
    /// Tracks all in-flight transactions
    in_flight_tracker: Arc<InFlightTracker>,
    /// BankForks for getting working bank for sanitization
    bank_forks: Arc<RwLock<BankForks>>,
    /// Senders for consuming transactions - 1 per worker
    consume_work_senders: Vec<Sender<ConsumeWork>>,
    /// Receiver for finished consumed transactions
    finished_consume_work_receiver: Receiver<FinishedConsumeWork>,
    /// Senders for forwarding transactions - shared for workers
    forward_work_sender: Sender<ForwardWork>,
    /// Receiver for finished forwarded transactions
    finished_forward_work_receiver: Receiver<FinishedForwardWork>,
    /// Receiver for packets from sigverify
    packet_receiver: BankingPacketReceiver,
    /// Generator for transaction ids
    transaction_id_generator: Arc<TransactionIdGenerator>,
    /// Generator for batch ids
    batch_id_generator: BatchIdGenerator,
}

impl MultiIteratorScheduler {
    pub fn new(
        num_threads: usize,
        decision_maker: DecisionMaker,
        bank_forks: Arc<RwLock<BankForks>>,
        consume_work_senders: Vec<Sender<ConsumeWork>>,
        finished_consume_work_receiver: Receiver<FinishedConsumeWork>,
        forward_work_sender: Sender<ForwardWork>,
        finished_forward_work_receiver: Receiver<FinishedForwardWork>,
        packet_receiver: BankingPacketReceiver,
    ) -> Self {
        Self {
            num_threads,
            thread_in_flight_limit: 6400, // ~100 batches
            decision_maker,
            account_locks: Arc::new(ThreadAwareAccountLocks::new(num_threads)),
            container: Arc::new(TransactionPacketContainer::with_capacity(700_000)),
            in_flight_tracker: Arc::new(InFlightTracker::new(num_threads)),
            bank_forks,
            consume_work_senders,
            finished_consume_work_receiver,
            forward_work_sender,
            finished_forward_work_receiver,
            packet_receiver,
            transaction_id_generator: Arc::default(),
            batch_id_generator: BatchIdGenerator::default(),
        }
    }

    pub fn run(self) -> Result<(), SchedulerError> {
        // Spawn sanitizing threads
        let exit = Arc::new(AtomicBool::new(false));
        let sanitizing_threads = (0..4)
            .map(|id| {
                let sanitizer = Sanitizer::new(
                    exit.clone(),
                    self.packet_receiver.clone(),
                    self.transaction_id_generator.clone(),
                    self.container.clone(),
                    self.bank_forks.clone(),
                );
                std::thread::Builder::new()
                    .name(format!("solSanitizer-{id:02}"))
                    .spawn(move || sanitizer.run())
                    .unwrap()
            })
            .collect_vec();

        // Spawn work finishing threads
        let work_finishing_threads = (0..4)
            .map(|id| {
                let work_finisher = WorkFinisher::new(
                    exit.clone(),
                    self.finished_consume_work_receiver.clone(),
                    self.finished_forward_work_receiver.clone(),
                    self.bank_forks.clone(),
                    self.account_locks.clone(),
                    self.container.clone(),
                    self.in_flight_tracker.clone(),
                );
                std::thread::Builder::new()
                    .name(format!("solWkFinisher-{id:02}"))
                    .spawn(move || work_finisher.run())
                    .unwrap()
            })
            .collect_vec();

        self.run_scheduler()?;

        exit.store(true, Ordering::Relaxed);
        for sanitizing_thread in sanitizing_threads {
            sanitizing_thread.join().unwrap();
        }
        for work_finishing_thread in work_finishing_threads {
            work_finishing_thread.join().unwrap();
        }

        Ok(())
    }

    fn run_scheduler(mut self) -> Result<(), SchedulerError> {
        loop {
            // If there are queued transactions/packets, make a decision about what to do with them
            // and schedule work accordingly
            if !self.container.is_empty() {
                let decision = self.decision_maker.make_consume_or_forward_decision();
                match decision {
                    BufferedPacketsDecision::Consume(bank_start) => {
                        self.schedule_consume(&bank_start.working_bank)?
                    }
                    BufferedPacketsDecision::Forward => self.schedule_forward(false)?,
                    BufferedPacketsDecision::ForwardAndHold => self.schedule_forward(true)?,
                    BufferedPacketsDecision::Hold => {}
                }
            }
        }
    }

    fn schedule_consume(&mut self, bank: &Bank) -> Result<(), SchedulerError> {
        // Take the top transactions from the priority queue
        // Note: we do not take all the transactions into a single batch
        //       because serialization time can be excessive when the queue
        //       is very large
        const MAX_TRANSACTIONS_PER_SCHEDULE_ITERATION: usize = 100_000;
        let transaction_ids = self
            .container
            .take_top_n(MAX_TRANSACTIONS_PER_SCHEDULE_ITERATION)
            .collect_vec();

        let mut scanner = MultiIteratorScanner::new(
            &transaction_ids,
            self.num_threads * MAX_NUM_TRANSACTIONS_PER_BATCH,
            ConsumePayload::new(bank, self),
            ConsumePayload::should_consume,
        );

        while let Some((_, payload)) = scanner.iterate() {
            let scheduler = &mut *payload.scheduler;
            for thread_id in 0..scheduler.num_threads {
                // Skip over threads that have no work scheduled
                if payload.transaction_batches[thread_id].is_empty() {
                    continue;
                }

                // Take ownership of constructed batches, replacing with equal capacity vectors
                let transaction_id_batch = std::mem::replace(
                    &mut payload.transaction_id_batches[thread_id],
                    Vec::with_capacity(MAX_NUM_TRANSACTIONS_PER_BATCH),
                );
                let transaction_batch = std::mem::replace(
                    &mut payload.transaction_batches[thread_id],
                    Vec::with_capacity(MAX_NUM_TRANSACTIONS_PER_BATCH),
                );
                let max_age_slot_batch = std::mem::replace(
                    &mut payload.max_age_slot_batches[thread_id],
                    Vec::with_capacity(MAX_NUM_TRANSACTIONS_PER_BATCH),
                );
                let batch_id = scheduler.batch_id_generator.next();
                scheduler.in_flight_tracker.track_batch(
                    batch_id,
                    transaction_batch.len(),
                    thread_id,
                );
                scheduler.consume_work_senders[thread_id]
                    .send(ConsumeWork {
                        batch_id,
                        ids: transaction_id_batch,
                        transactions: transaction_batch,
                        max_age_slots: max_age_slot_batch,
                    })
                    .map_err(|_| SchedulerError::DisconnectedSendChannel("consume work sender"))?;
            }

            // Reset the payload for next set of batches
            payload.reset();
        }

        // If a transaction was not consumed, due to unschedulable lock conflicts, then
        // it should be re-added into the priority queue.
        let already_processed = scanner.finalize().already_handled;
        self.reinsert_unschedulable_ids(transaction_ids, already_processed);

        Ok(())
    }

    fn schedule_forward(&mut self, hold: bool) -> Result<(), SchedulerError> {
        let transaction_priority_ids = self.container.drain_queue().collect_vec();
        let bank = self.bank_forks.read().unwrap().root_bank();

        let mut scanner = MultiIteratorScanner::new(
            &transaction_priority_ids,
            MAX_NUM_TRANSACTIONS_PER_BATCH,
            ForwardPayload::new(&self.container, &bank),
            ForwardPayload::should_forward,
        );

        while let Some((batch_transaction_priority_ids, payload)) = scanner.iterate() {
            let ids = batch_transaction_priority_ids
                .iter()
                .map(|priority_id| priority_id.id)
                .collect_vec();

            let packets = if hold {
                // Keep both transaction and packet inside the scheduler container
                ids.iter()
                    .map(|id| {
                        payload
                            .container
                            .get_packet_entry(*id)
                            .unwrap()
                            .get()
                            .immutable_section()
                            .clone()
                    })
                    .collect_vec()
            } else {
                // Remove both transaction and packet from the scheduler container
                ids.iter()
                    .map(|id| {
                        let packet = payload
                            .container
                            .get_packet_entry(*id)
                            .unwrap()
                            .get()
                            .immutable_section()
                            .clone();
                        payload.container.remove_by_id(id);
                        packet
                    })
                    .collect_vec()
            };

            self.forward_work_sender
                .send(ForwardWork { ids, packets })
                .map_err(|_| SchedulerError::DisconnectedSendChannel("forward work sender"))?;

            // Reset the payload for next set of batches
            payload.reset();
        }

        // If a transaction was not consumed, due to unschedulable lock conflicts, then
        // it should be re-added into the priority queue.
        let already_processed = scanner.finalize().already_handled;
        self.reinsert_unschedulable_ids(transaction_priority_ids, already_processed);

        Ok(())
    }

    fn reinsert_unschedulable_ids(
        &mut self,
        transaction_ids: Vec<TransactionPriorityId>,
        already_processed: Vec<bool>,
    ) {
        transaction_ids
            .into_iter()
            .zip(already_processed.into_iter())
            .filter(|(_, already_processed)| !already_processed)
            .map(|(id, _)| id)
            .for_each(|id| {
                self.container.push_id_into_queue(id);
            })
    }

    fn should_retry_expired_transaction(
        packet: &ImmutableDeserializedPacket,
        bank: &Bank,
        r_blockhash_queue: &BlockhashQueue,
    ) -> Option<SanitizedTransaction> {
        // Check age
        r_blockhash_queue
            .get_hash_age(
                packet
                    .transaction()
                    .get_message()
                    .message
                    .recent_blockhash(),
            )
            .filter(|age| *age <= MAX_PROCESSING_AGE as u64)?;

        packet.build_sanitized_transaction(&bank.feature_set, bank.vote_only_bank(), bank)
    }
}

struct ConsumePayload<'a> {
    bank: &'a Bank,
    blockhash_queue: RwLockReadGuard<'a, BlockhashQueue>,
    last_slot_in_epoch: Slot,
    scheduler: &'a mut MultiIteratorScheduler,
    batch_account_locks: ReadWriteAccountSet,
    transaction_id_batches: Vec<Vec<TransactionId>>,
    transaction_batches: Vec<Vec<SanitizedTransaction>>,
    max_age_slot_batches: Vec<Vec<Slot>>,
    schedulable_threads: ThreadSet, // threads that don't have a full batch yet
}

impl<'a> ConsumePayload<'a> {
    fn new(bank: &'a Bank, scheduler: &'a mut MultiIteratorScheduler) -> Self {
        let num_threads = scheduler.num_threads;
        let blockhash_queue = bank.read_blockhash_queue().unwrap();
        let last_slot_in_epoch = bank.epoch_schedule().get_last_slot_in_epoch(bank.slot());
        Self {
            bank,
            blockhash_queue,
            last_slot_in_epoch,
            scheduler,
            batch_account_locks: ReadWriteAccountSet::default(),
            transaction_id_batches: vec![
                Vec::with_capacity(MAX_NUM_TRANSACTIONS_PER_BATCH);
                num_threads
            ],
            transaction_batches: vec![
                Vec::with_capacity(MAX_NUM_TRANSACTIONS_PER_BATCH);
                num_threads
            ],
            max_age_slot_batches: vec![
                Vec::with_capacity(MAX_NUM_TRANSACTIONS_PER_BATCH);
                num_threads
            ],
            schedulable_threads: ThreadSet::any(num_threads),
        }
    }

    fn reset(&mut self) {
        self.batch_account_locks.clear();
        self.schedulable_threads = ThreadSet::any(self.scheduler.num_threads);
        // Don't allow the scheduler to send more than the limit
        for thread_id in 0..self.scheduler.num_threads {
            if self
                .scheduler
                .in_flight_tracker
                .num_in_flight_for_thread(thread_id)
                >= self.scheduler.thread_in_flight_limit
            {
                self.schedulable_threads.remove(thread_id);
            }
        }

        // We don't need to clear these here because we are already going to take the memory
        // when we send to the workers
        // self.transaction_id_batches.iter_mut().for_each(Vec::clear);
        // self.transaction_batches.iter_mut().for_each(Vec::clear);
    }

    fn should_consume(
        priority_id: &TransactionPriorityId,
        payload: &mut Self,
    ) -> ProcessingDecision {
        let scheduler = &mut *payload.scheduler;

        let (mut transaction_entry, packet_entry) = scheduler
            .container
            .get_transaction_and_packet_entries(priority_id.id);

        // If expired - check if the transaction can be re-sanitized
        if transaction_entry.get().max_age_slot < payload.bank.slot() {
            if let Some(resanitized_transaction) =
                MultiIteratorScheduler::should_retry_expired_transaction(
                    packet_entry.get().immutable_section(),
                    payload.bank,
                    &payload.blockhash_queue,
                )
            {
                let mut entry = transaction_entry.get_mut();
                entry.max_age_slot = payload.last_slot_in_epoch;
                entry.transaction = resanitized_transaction;
            } else {
                transaction_entry.remove();
                packet_entry.remove();
                return ProcessingDecision::Never;
            }
        }

        let SanitizedTransactionTTL { transaction, .. } = transaction_entry.get();

        let account_locks = transaction.get_account_locks_unchecked();

        // Check if the transaction conflicts with any transactions in the current batch
        if !payload
            .batch_account_locks
            .check_sanitized_message_account_locks(transaction.message())
        {
            return ProcessingDecision::Later;
        }

        let outstanding_locks = &mut scheduler.account_locks;
        let batches = &payload.transaction_batches;
        let in_flight = scheduler.in_flight_tracker.num_in_flight_per_thread();
        let Some(thread_id) = outstanding_locks.try_lock_accounts(
            account_locks.writable.into_iter(),
            account_locks.readonly.into_iter(),
            payload.schedulable_threads,
            |thread_set| Self::select_thread(batches, in_flight, thread_set),
        ) else {
            return ProcessingDecision::Later;
        };

        // Add the locks to the current batch
        payload
            .batch_account_locks
            .add_sanitized_message_account_locks(transaction.message());

        // remove from container while it is in-flight
        let SanitizedTransactionTTL {
            transaction,
            max_age_slot,
        } = transaction_entry.remove();
        // Update our payload to include this transaction
        payload.transaction_id_batches[thread_id].push(priority_id.id);
        payload.transaction_batches[thread_id].push(transaction);
        payload.max_age_slot_batches[thread_id].push(max_age_slot);
        if payload.transaction_batches[thread_id].len() == MAX_NUM_TRANSACTIONS_PER_BATCH {
            payload.schedulable_threads.remove(thread_id);
        }

        // Don't allow further scheduling if this thread is at the limit
        if payload.transaction_batches[thread_id].len()
            + in_flight[thread_id].load(Ordering::Relaxed)
            >= scheduler.thread_in_flight_limit
        {
            payload.schedulable_threads.remove(thread_id);
        }

        ProcessingDecision::Now
    }

    fn select_thread(
        batches_per_thread: &[Vec<SanitizedTransaction>],
        in_flight_per_thread: &[AtomicUsize],
        thread_set: ThreadSet,
    ) -> ThreadId {
        thread_set
            .contained_threads_iter()
            .map(|thread_id| {
                (
                    thread_id,
                    batches_per_thread[thread_id].len()
                        + in_flight_per_thread[thread_id].load(Ordering::Relaxed),
                )
            })
            .min_by(|a, b| a.1.cmp(&b.1))
            .map(|(thread_id, _)| thread_id)
            .unwrap()
    }
}

struct ForwardPayload<'a> {
    /// Account locks used to prevent us from spam forwarding hot accounts
    account_locks: ReadWriteAccountSet,

    container: &'a TransactionPacketContainer,
    bank: &'a Bank,
    blockhash_queue: RwLockReadGuard<'a, BlockhashQueue>,
    status_cache: RwLockReadGuard<'a, BankStatusCache>,
    next_durable_nonce: DurableNonce,
    max_age: usize,
    error_counters: TransactionErrorMetrics,
}

impl<'a> ForwardPayload<'a> {
    fn new(container: &'a TransactionPacketContainer, bank: &'a Bank) -> Self {
        let blockhash_queue = bank.read_blockhash_queue().unwrap();
        let status_cache = bank.status_cache.read().unwrap();
        let next_durable_nonce = DurableNonce::from_blockhash(&blockhash_queue.last_hash());
        // Calculate max forwarding age
        let max_age = (MAX_PROCESSING_AGE)
            .saturating_sub(if perf_libs::api().is_some() {
                MAX_TRANSACTION_FORWARDING_DELAY
            } else {
                MAX_TRANSACTION_FORWARDING_DELAY_GPU
            })
            .saturating_sub(FORWARD_TRANSACTIONS_TO_LEADER_AT_SLOT_OFFSET as usize);

        Self {
            account_locks: ReadWriteAccountSet::default(),
            container,
            bank,
            blockhash_queue,
            status_cache,
            next_durable_nonce,
            max_age,
            error_counters: TransactionErrorMetrics::default(),
        }
    }

    fn reset(&mut self) {
        self.account_locks.clear();
    }

    fn should_forward(
        priority_id: &TransactionPriorityId,
        payload: &mut Self,
    ) -> ProcessingDecision {
        let transaction_entry = payload.container.get_transaction_entry(priority_id.id);
        let SanitizedTransactionTTL {
            transaction,
            max_age_slot,
        } = transaction_entry.get();

        // If the transaction is too old, we don't forward it
        if *max_age_slot < payload.bank.slot()
            || payload
                .bank
                .check_transaction_age(
                    transaction,
                    payload.max_age,
                    &payload.next_durable_nonce,
                    &payload.blockhash_queue,
                    &mut payload.error_counters,
                )
                .0
                .is_err()
        {
            drop(transaction_entry);
            payload.container.remove_by_id(&priority_id.id);
            return ProcessingDecision::Never;
        }

        // If the transaction is already in the bank we don't forward it
        if payload
            .bank
            .is_transaction_already_processed(transaction, &payload.status_cache)
        {
            payload.error_counters.already_processed += 1;
            return ProcessingDecision::Never;
        }

        // If locks clash with the current batch of transactions, then we should forward
        // the transaction later.
        if payload.account_locks.try_locking(transaction.message()) {
            ProcessingDecision::Now
        } else {
            ProcessingDecision::Later
        }
    }
}

#[cfg(test)]
mod tests {
    use {
        super::*,
        crate::{
            banking_stage::{
                consumer::MAX_NUM_TRANSACTIONS_PER_BATCH, tests::create_slow_genesis_config,
            },
            banking_trace::BankingPacketBatch,
            sigverify::SigverifyTracerPacketStats,
        },
        crossbeam_channel::unbounded,
        solana_ledger::{
            blockstore::Blockstore, genesis_utils::GenesisConfigInfo,
            get_tmp_ledger_path_auto_delete, leader_schedule_cache::LeaderScheduleCache,
        },
        solana_perf::packet::{to_packet_batches, PacketBatch, NUM_PACKETS},
        solana_poh::poh_recorder::{PohRecorder, Record, WorkingBankEntry},
        solana_runtime::bank_forks::BankForks,
        solana_sdk::{
            compute_budget::ComputeBudgetInstruction, hash::Hash, message::Message,
            poh_config::PohConfig, pubkey::Pubkey, signature::Keypair, signer::Signer,
            system_instruction, transaction::Transaction,
        },
        std::{
            sync::{atomic::AtomicBool, Arc, RwLock},
            time::Duration,
        },
        tempfile::TempDir,
    };

    const TEST_TIMEOUT: Duration = Duration::from_millis(1000);

    fn create_channels<T>(num: usize) -> (Vec<Sender<T>>, Vec<Receiver<T>>) {
        (0..num).map(|_| unbounded()).unzip()
    }

    // Helper struct to create tests that hold channels, files, etc.
    // such that our tests can be more easily set up and run.
    struct TestFrame {
        bank: Arc<Bank>,
        ledger_path: TempDir,
        entry_receiver: Receiver<WorkingBankEntry>,
        record_receiver: Receiver<Record>,
        poh_recorder: Arc<RwLock<PohRecorder>>,
        banking_packet_sender: Sender<Arc<(Vec<PacketBatch>, Option<SigverifyTracerPacketStats>)>>,

        consume_work_receivers: Vec<Receiver<ConsumeWork>>,
        finished_consume_work_sender: Sender<FinishedConsumeWork>,
        forward_work_receiver: Receiver<ForwardWork>,
        finished_forward_work_sender: Sender<FinishedForwardWork>,
    }

    fn create_test_frame(num_threads: usize) -> (TestFrame, MultiIteratorScheduler) {
        let GenesisConfigInfo { genesis_config, .. } = create_slow_genesis_config(10_000);
        let bank = Bank::new_no_wallclock_throttle_for_tests(&genesis_config);
        let bank_forks = Arc::new(RwLock::new(BankForks::new(bank)));
        let bank = bank_forks.read().unwrap().working_bank();

        let ledger_path = get_tmp_ledger_path_auto_delete!();
        let blockstore = Blockstore::open(ledger_path.path())
            .expect("Expected to be able to open database ledger");
        let (poh_recorder, entry_receiver, record_receiver) = PohRecorder::new(
            bank.tick_height(),
            bank.last_blockhash(),
            bank.clone(),
            Some((4, 4)),
            bank.ticks_per_slot(),
            &Pubkey::new_unique(),
            Arc::new(blockstore),
            &Arc::new(LeaderScheduleCache::new_from_bank(&bank)),
            &PohConfig::default(),
            Arc::new(AtomicBool::default()),
        );
        let poh_recorder = Arc::new(RwLock::new(poh_recorder));
        let decision_maker = DecisionMaker::new(Pubkey::new_unique(), poh_recorder.clone());

        let (banking_packet_sender, banking_packet_receiver) = unbounded();

        let (consume_work_senders, consume_work_receivers) = create_channels(num_threads);
        let (finished_consume_work_sender, finished_consume_work_receiver) = unbounded();
        let (forward_work_sender, forward_work_receiver) = unbounded();
        let (finished_forward_work_sender, finished_forward_work_receiver) = unbounded();

        let test_frame = TestFrame {
            bank,
            ledger_path,
            entry_receiver,
            record_receiver,
            poh_recorder,
            banking_packet_sender,
            consume_work_receivers,
            finished_consume_work_sender,
            forward_work_receiver,
            finished_forward_work_sender,
        };
        let multi_iterator_scheduler = MultiIteratorScheduler::new(
            num_threads,
            decision_maker,
            bank_forks,
            consume_work_senders,
            finished_consume_work_receiver,
            forward_work_sender,
            finished_forward_work_receiver,
            banking_packet_receiver,
        );

        (test_frame, multi_iterator_scheduler)
    }

    fn prioritized_tranfer(
        from_keypair: &Keypair,
        to_pubkey: &Pubkey,
        lamports: u64,
        priority: u64,
        recent_blockhash: Hash,
    ) -> Transaction {
        let transfer = system_instruction::transfer(&from_keypair.pubkey(), to_pubkey, lamports);
        let prioritization = ComputeBudgetInstruction::set_compute_unit_price(priority);
        let message = Message::new(&[transfer, prioritization], Some(&from_keypair.pubkey()));
        Transaction::new(&vec![from_keypair], message, recent_blockhash)
    }

    fn to_banking_packet_batch(txs: &[Transaction]) -> BankingPacketBatch {
        let packet_batch = to_packet_batches(txs, NUM_PACKETS);
        Arc::new((packet_batch, None))
    }

    #[test]
    fn test_schedule_consume_single_threaded_no_conflicts() {
        let (test_frame, multi_iterator_scheduler) = create_test_frame(1);
        let TestFrame {
            bank,
            poh_recorder,
            banking_packet_sender,
            consume_work_receivers,
            ..
        } = &test_frame;

        let scheduler_thread = std::thread::spawn(move || multi_iterator_scheduler.run());

        // Send packet batch to the scheduler - should do nothing until we become the leader.
        let tx1 = prioritized_tranfer(
            &Keypair::new(),
            &Pubkey::new_unique(),
            1,
            1,
            bank.last_blockhash(),
        );
        let tx2 = prioritized_tranfer(
            &Keypair::new(),
            &Pubkey::new_unique(),
            1,
            2,
            bank.last_blockhash(),
        );
        let tx1_hash = tx1.message().hash();
        let tx2_hash = tx2.message().hash();

        let txs = vec![tx1, tx2];
        banking_packet_sender
            .send(to_banking_packet_batch(&txs))
            .unwrap();

        // set bank
        poh_recorder.write().unwrap().set_bank(bank, false);
        let consume_work = consume_work_receivers[0]
            .recv_timeout(TEST_TIMEOUT)
            .unwrap();
        assert_eq!(consume_work.ids.len(), 2);
        assert_eq!(consume_work.transactions.len(), 2);
        let message_hashes = consume_work
            .transactions
            .iter()
            .map(|tx| tx.message_hash())
            .collect_vec();
        assert_eq!(message_hashes, vec![&tx2_hash, &tx1_hash]);

        drop(test_frame);
        let _ = scheduler_thread.join();
    }

    #[test]
    fn test_schedule_consume_single_threaded_no_conflicts_in_progress() {
        let (test_frame, multi_iterator_scheduler) = create_test_frame(1);
        let TestFrame {
            bank,
            poh_recorder,
            banking_packet_sender,
            consume_work_receivers,
            ..
        } = &test_frame;

        let scheduler_thread = std::thread::spawn(move || multi_iterator_scheduler.run());

        // set bank before sending packets - should still be scheduled even while already leader
        poh_recorder.write().unwrap().set_bank(bank, false);

        // Send packet batch to the scheduler - should do nothing until we become the leader.
        let tx1 = prioritized_tranfer(
            &Keypair::new(),
            &Pubkey::new_unique(),
            1,
            1,
            bank.last_blockhash(),
        );
        let tx2 = prioritized_tranfer(
            &Keypair::new(),
            &Pubkey::new_unique(),
            1,
            2,
            bank.last_blockhash(),
        );
        let tx1_hash = tx1.message().hash();
        let tx2_hash = tx2.message().hash();

        let txs = vec![tx1, tx2];
        banking_packet_sender
            .send(to_banking_packet_batch(&txs))
            .unwrap();

        let consume_work = consume_work_receivers[0]
            .recv_timeout(TEST_TIMEOUT)
            .unwrap();
        assert_eq!(consume_work.ids.len(), 2);
        assert_eq!(consume_work.transactions.len(), 2);
        let message_hashes = consume_work
            .transactions
            .iter()
            .map(|tx| tx.message_hash())
            .collect_vec();
        // transactions appear in priority order - even though there are no conflicts
        assert_eq!(message_hashes, vec![&tx2_hash, &tx1_hash]);

        drop(test_frame);
        let _ = scheduler_thread.join();
    }

    #[test]
    fn test_schedule_consume_single_threaded_conflict() {
        let (test_frame, multi_iterator_scheduler) = create_test_frame(1);
        let TestFrame {
            bank,
            poh_recorder,
            banking_packet_sender,
            consume_work_receivers,
            ..
        } = &test_frame;

        let scheduler_thread = std::thread::spawn(move || multi_iterator_scheduler.run());
        poh_recorder.write().unwrap().set_bank(bank, false);

        let pk = Pubkey::new_unique();
        let tx1 = prioritized_tranfer(&Keypair::new(), &pk, 1, 1, bank.last_blockhash());
        let tx2 = prioritized_tranfer(&Keypair::new(), &pk, 1, 2, bank.last_blockhash());
        let tx1_hash = tx1.message().hash();
        let tx2_hash = tx2.message().hash();

        let txs = vec![tx1, tx2];
        banking_packet_sender
            .send(to_banking_packet_batch(&txs))
            .unwrap();

        // We expect 2 batches to be scheduled
        let consume_works = (0..2)
            .map(|_| {
                consume_work_receivers[0]
                    .recv_timeout(TEST_TIMEOUT)
                    .unwrap()
            })
            .collect_vec();

        let num_txs_per_batch = consume_works.iter().map(|cw| cw.ids.len()).collect_vec();
        let message_hashes = consume_works
            .iter()
            .flat_map(|cw| cw.transactions.iter().map(|tx| tx.message_hash()))
            .collect_vec();
        assert_eq!(num_txs_per_batch, vec![1; 2]);
        assert_eq!(message_hashes, vec![&tx2_hash, &tx1_hash]);

        drop(test_frame);
        let _ = scheduler_thread.join();
    }

    #[test]
    fn test_schedule_consume_single_threaded_multi_batch() {
        let (test_frame, multi_iterator_scheduler) = create_test_frame(1);
        let TestFrame {
            bank,
            poh_recorder,
            banking_packet_sender,
            consume_work_receivers,
            ..
        } = &test_frame;

        let scheduler_thread = std::thread::spawn(move || multi_iterator_scheduler.run());

        // Send multiple batches - all get scheduled
        let txs1 = (0..2 * MAX_NUM_TRANSACTIONS_PER_BATCH)
            .map(|i| {
                prioritized_tranfer(
                    &Keypair::new(),
                    &Pubkey::new_unique(),
                    i as u64,
                    1,
                    bank.last_blockhash(),
                )
            })
            .collect_vec();
        let txs2 = (0..2 * MAX_NUM_TRANSACTIONS_PER_BATCH)
            .map(|i| {
                prioritized_tranfer(
                    &Keypair::new(),
                    &Pubkey::new_unique(),
                    i as u64,
                    2,
                    bank.last_blockhash(),
                )
            })
            .collect_vec();

        banking_packet_sender
            .send(to_banking_packet_batch(&txs1))
            .unwrap();
        banking_packet_sender
            .send(to_banking_packet_batch(&txs2))
            .unwrap();
        poh_recorder.write().unwrap().set_bank(bank, false);

        // We expect 4 batches to be scheduled
        let consume_works = (0..4)
            .map(|_| {
                consume_work_receivers[0]
                    .recv_timeout(TEST_TIMEOUT)
                    .unwrap()
            })
            .collect_vec();

        assert_eq!(
            consume_works.iter().map(|cw| cw.ids.len()).collect_vec(),
            vec![MAX_NUM_TRANSACTIONS_PER_BATCH; 4]
        );

        drop(test_frame);
        let _ = scheduler_thread.join();
    }

    #[test]
    fn test_schedule_consume_simple_thread_selection() {
        let (test_frame, multi_iterator_scheduler) = create_test_frame(2);
        let TestFrame {
            bank,
            poh_recorder,
            banking_packet_sender,
            consume_work_receivers,
            ..
        } = &test_frame;
        let scheduler_thread = std::thread::spawn(move || multi_iterator_scheduler.run());
        poh_recorder.write().unwrap().set_bank(bank, false);

        // Send 4 transactions w/o conflicts. 2 should be scheduled on each thread
        let txs = (0..4)
            .map(|i| {
                prioritized_tranfer(
                    &Keypair::new(),
                    &Pubkey::new_unique(),
                    1,
                    i,
                    bank.last_blockhash(),
                )
            })
            .collect_vec();
        banking_packet_sender
            .send(to_banking_packet_batch(&txs))
            .unwrap();

        // Priority Expectation:
        // Thread 0: [3, 1]
        // Thread 1: [2, 0]
        let t0_expected = [3, 1]
            .into_iter()
            .map(|i| txs[i].message().hash())
            .collect_vec();
        let t1_expected = [2, 0]
            .into_iter()
            .map(|i| txs[i].message().hash())
            .collect_vec();
        let t0_actual = consume_work_receivers[0]
            .recv_timeout(TEST_TIMEOUT)
            .unwrap()
            .transactions
            .iter()
            .map(|tx| *tx.message_hash())
            .collect_vec();
        let t1_actual = consume_work_receivers[1]
            .recv_timeout(TEST_TIMEOUT)
            .unwrap()
            .transactions
            .iter()
            .map(|tx| *tx.message_hash())
            .collect_vec();

        assert_eq!(t0_actual, t0_expected);
        assert_eq!(t1_actual, t1_expected);

        drop(test_frame);
        let _ = scheduler_thread.join();
    }

    #[test]
    fn test_schedule_consume_non_schedulable() {
        let (test_frame, multi_iterator_scheduler) = create_test_frame(2);
        let TestFrame {
            bank,
            poh_recorder,
            banking_packet_sender,
            consume_work_receivers,
            finished_consume_work_sender,
            ..
        } = &test_frame;
        let scheduler_thread = std::thread::spawn(move || multi_iterator_scheduler.run());
        poh_recorder.write().unwrap().set_bank(bank, false);

        let accounts = (0..4).map(|_| Keypair::new()).collect_vec();

        // high priority transactions [0, 1] do not conflict, and should be
        // scheduled to *different* threads.
        // low priority transaction [2] conflicts with both, and thus will
        // not be schedulable until one of the previous transactions is
        // completed.
        let txs = vec![
            prioritized_tranfer(
                &accounts[0],
                &accounts[1].pubkey(),
                1,
                2,
                bank.last_blockhash(),
            ),
            prioritized_tranfer(
                &accounts[2],
                &accounts[3].pubkey(),
                1,
                1,
                bank.last_blockhash(),
            ),
            prioritized_tranfer(
                &accounts[1],
                &accounts[2].pubkey(),
                1,
                0,
                bank.last_blockhash(),
            ),
        ];
        banking_packet_sender
            .send(to_banking_packet_batch(&txs))
            .unwrap();

        // Initial batches expectation:
        // Thread 0: [3, 1]
        // Thread 1: [2, 0]
        let t0_expected = [0]
            .into_iter()
            .map(|i| txs[i].message().hash())
            .collect_vec();
        let t1_expected = [1]
            .into_iter()
            .map(|i| txs[i].message().hash())
            .collect_vec();
        let t0_work = consume_work_receivers[0]
            .recv_timeout(TEST_TIMEOUT)
            .unwrap();
        let t1_work = consume_work_receivers[1]
            .recv_timeout(TEST_TIMEOUT)
            .unwrap();

        let t0_actual = t0_work
            .transactions
            .iter()
            .map(|tx| *tx.message_hash())
            .collect_vec();
        let t1_actual = t1_work
            .transactions
            .iter()
            .map(|tx| *tx.message_hash())
            .collect_vec();

        assert_eq!(t0_actual, t0_expected);
        assert_eq!(t1_actual, t1_expected);

        // Complete t1's batch - t0 should not be schedulable
        finished_consume_work_sender
            .send(FinishedConsumeWork {
                work: t1_work,
                retryable_indexes: vec![],
            })
            .unwrap();

        // t0 should not be scheduled for the remaining transaction
        let remaining_expected = [2]
            .into_iter()
            .map(|i| txs[i].message().hash())
            .collect_vec();
        let remaining_actual = consume_work_receivers[0]
            .recv_timeout(TEST_TIMEOUT)
            .unwrap()
            .transactions
            .iter()
            .map(|tx| *tx.message_hash())
            .collect_vec();
        assert_eq!(remaining_actual, remaining_expected);

        drop(test_frame);
        let _ = scheduler_thread.join();
    }

    #[test]
    fn test_schedule_consume_retryable() {
        let (test_frame, multi_iterator_scheduler) = create_test_frame(1);
        let TestFrame {
            bank,
            poh_recorder,
            banking_packet_sender,
            consume_work_receivers,
            finished_consume_work_sender,
            ..
        } = &test_frame;

        let scheduler_thread = std::thread::spawn(move || multi_iterator_scheduler.run());

        // Send packet batch to the scheduler - should do nothing until we become the leader.
        let tx1 = prioritized_tranfer(
            &Keypair::new(),
            &Pubkey::new_unique(),
            1,
            1,
            bank.last_blockhash(),
        );
        let tx2 = prioritized_tranfer(
            &Keypair::new(),
            &Pubkey::new_unique(),
            1,
            2,
            bank.last_blockhash(),
        );
        let tx1_hash = tx1.message().hash();
        let tx2_hash = tx2.message().hash();

        let txs = vec![tx1, tx2];
        banking_packet_sender
            .send(to_banking_packet_batch(&txs))
            .unwrap();

        // set bank
        poh_recorder.write().unwrap().set_bank(bank, false);
        let consume_work = consume_work_receivers[0]
            .recv_timeout(TEST_TIMEOUT)
            .unwrap();
        assert_eq!(consume_work.ids.len(), 2);
        assert_eq!(consume_work.transactions.len(), 2);
        let message_hashes = consume_work
            .transactions
            .iter()
            .map(|tx| tx.message_hash())
            .collect_vec();
        assert_eq!(message_hashes, vec![&tx2_hash, &tx1_hash]);

        // Complete the batch - marking the second transaction as retryable
        finished_consume_work_sender
            .send(FinishedConsumeWork {
                work: consume_work,
                retryable_indexes: vec![1],
            })
            .unwrap();

        // Transaction should be rescheduled
        let consume_work = consume_work_receivers[0]
            .recv_timeout(TEST_TIMEOUT)
            .unwrap();
        assert_eq!(consume_work.ids.len(), 1);
        assert_eq!(consume_work.transactions.len(), 1);
        let message_hashes = consume_work
            .transactions
            .iter()
            .map(|tx| tx.message_hash())
            .collect_vec();
        assert_eq!(message_hashes, vec![&tx1_hash]);

        drop(test_frame);
        let _ = scheduler_thread.join();
    }

    #[test]
    fn test_schedule_forward() {
        let (test_frame, multi_iterator_scheduler) = create_test_frame(1);
        let TestFrame {
            bank,
            banking_packet_sender,
            forward_work_receiver,
            finished_forward_work_sender,
            ..
        } = &test_frame;

        let scheduler_thread = std::thread::spawn(move || multi_iterator_scheduler.run());

        // Send multiple batches - all get scheduled
        let txs1 = (0..2 * MAX_NUM_TRANSACTIONS_PER_BATCH)
            .map(|i| {
                prioritized_tranfer(
                    &Keypair::new(),
                    &Pubkey::new_unique(),
                    i as u64,
                    1,
                    bank.last_blockhash(),
                )
            })
            .collect_vec();
        let txs2 = (0..2 * MAX_NUM_TRANSACTIONS_PER_BATCH)
            .map(|i| {
                prioritized_tranfer(
                    &Keypair::new(),
                    &Pubkey::new_unique(),
                    i as u64,
                    2,
                    bank.last_blockhash(),
                )
            })
            .collect_vec();

        banking_packet_sender
            .send(to_banking_packet_batch(&txs1))
            .unwrap();
        banking_packet_sender
            .send(to_banking_packet_batch(&txs2))
            .unwrap();

        // We expect 4 batches to be scheduled
        let forward_works = (0..4)
            .map(|_| forward_work_receiver.recv_timeout(TEST_TIMEOUT).unwrap())
            .collect_vec();

        assert_eq!(
            forward_works.iter().map(|cw| cw.ids.len()).collect_vec(),
            vec![MAX_NUM_TRANSACTIONS_PER_BATCH; 4]
        );
        for forward_work in forward_works.into_iter() {
            finished_forward_work_sender
                .send(FinishedForwardWork {
                    work: forward_work,
                    successful: true,
                })
                .unwrap();
        }

        drop(test_frame);
        let _ = scheduler_thread.join();
    }

    #[test]
    fn test_schedule_forward_conflicts() {
        let (test_frame, multi_iterator_scheduler) = create_test_frame(1);
        let TestFrame {
            bank,
            banking_packet_sender,
            forward_work_receiver,
            ..
        } = &test_frame;

        let scheduler_thread = std::thread::spawn(move || multi_iterator_scheduler.run());

        let keypair1 = Keypair::new();
        let keypair2 = Keypair::new();

        let txs = vec![
            prioritized_tranfer(&keypair1, &keypair2.pubkey(), 1, 2, bank.last_blockhash()),
            prioritized_tranfer(&keypair2, &keypair1.pubkey(), 1, 1, bank.last_blockhash()),
        ];

        banking_packet_sender
            .send(to_banking_packet_batch(&txs))
            .unwrap();

        // We expect 2 batches to be scheduled since the transactions conflict
        let forward_works = (0..2)
            .map(|_| forward_work_receiver.recv_timeout(TEST_TIMEOUT).unwrap())
            .collect_vec();

        let expected_hashes = txs.iter().map(|tx| tx.message().hash()).collect_vec();
        let actual_hashes = forward_works
            .iter()
            .flat_map(|fw| fw.packets.iter())
            .map(|p| *p.message_hash())
            .collect_vec();
        assert_eq!(expected_hashes, actual_hashes,);

        drop(test_frame);
        let _ = scheduler_thread.join();
    }
}
