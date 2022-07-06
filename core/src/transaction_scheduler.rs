//! Implements a transaction scheduler

use std::{
    hash::Hash,
    sync::atomic::{AtomicBool, Ordering},
};

use crossbeam_channel::{select, Sender};

use {
    crate::unprocessed_packet_batches::{self, DeserializedPacket, ImmutableDeserializedPacket},
    crossbeam_channel::Receiver,
    solana_perf::packet::PacketBatch,
    solana_runtime::bank::Bank,
    solana_sdk::{pubkey::Pubkey, signature::Signature, transaction::SanitizedTransaction},
    std::{
        collections::{BTreeSet, BinaryHeap, HashMap, HashSet},
        rc::Rc,
        sync::Arc,
    },
};

/// Wrapper to store a sanitized transaction and priority
#[derive(Clone, Debug)]
struct TransactionPriority {
    /// Transaction priority
    priority: u64,
    /// Sanitized transaction
    transaction: SanitizedTransaction,
}

impl Ord for TransactionPriority {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.priority.cmp(&other.priority)
    }
}

impl PartialOrd for TransactionPriority {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        self.priority.partial_cmp(&other.priority)
    }
}

impl PartialEq for TransactionPriority {
    fn eq(&self, other: &Self) -> bool {
        self.priority == other.priority
    }
}

impl Eq for TransactionPriority {}

impl Hash for TransactionPriority {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        self.transaction.signature().hash(state);
    }
}

type TransactionRef = Arc<TransactionPriority>;

impl TransactionPriority {
    fn try_new(packet: &ImmutableDeserializedPacket, bank: &Bank) -> Option<TransactionRef> {
        let priority = packet.priority();
        let transaction = SanitizedTransaction::try_new(
            packet.transaction().clone(),
            *packet.message_hash(),
            packet.is_simple_vote(),
            bank,
        )
        .ok()?;
        transaction.verify_precompiles(&bank.feature_set).ok()?;
        Some(Arc::new(Self {
            transaction,
            priority,
        }))
    }
}

type PacketBatchMessage = Vec<PacketBatch>;
type TransactionMessage = TransactionRef;
type TransactionBatchMessage = Vec<TransactionMessage>;

/// Stores state for scheduling transactions and channels for communicating
/// with other threads: SigVerify and Banking
pub struct TransactionScheduler {
    /// Channel for receiving deserialized packet batches from SigVerify
    packet_batch_receiver: Receiver<PacketBatchMessage>,
    /// Channels for sending transaction batches to banking threads
    transaction_batch_senders: Vec<Sender<TransactionBatchMessage>>,
    /// Channel for receiving completed transactions from any banking thread
    completed_transaction_receiver: Receiver<TransactionMessage>,
    /// Bank that we are currently scheduling for
    bank: Arc<Bank>,
    /// Max number of transactions to send to a single banking-thread in a batch
    max_batch_size: usize,
    /// Exit signal
    exit: Arc<AtomicBool>,

    /// Pending transactions that are not known to be blocked
    pending_transactions: BinaryHeap<TransactionRef>,
    /// Transaction queues and locks by account key
    transactions_by_account: HashMap<Pubkey, AccountTransactionQueue>,
    /// Map from transaction signature to transactions blocked by the signature
    blocked_transactions: HashMap<Signature, HashSet<TransactionRef>>,
}

impl TransactionScheduler {
    /// Driving loop
    fn main(mut self) {
        loop {
            if self.exit.load(Ordering::Relaxed) {
                break;
            }
            self.iter();
        }
    }

    /// Performs work in a loop - Handles different channel receives/timers and performs scheduling
    fn iter(&mut self) {
        if let Ok(packet_batch_message) = self.packet_batch_receiver.try_recv() {
            self.handle_packet_batches(packet_batch_message);
        }
        if let Ok(completed_transaction) = self.completed_transaction_receiver.try_recv() {
            self.handle_completed_transaction(completed_transaction);
        }
        self.do_scheduling();
    }

    /// Performs scheduling operations on currently pending transactions
    fn do_scheduling(&mut self) {
        // Allocate batches to be sent to threads
        let mut batches =
            vec![Vec::with_capacity(self.max_batch_size); self.transaction_batch_senders.len()];

        // Do scheduling work
        let mut batch_index = 0;
        while let Some(transaction) = self.pending_transactions.pop() {
            if self.try_schedule_transaction(transaction, &mut batches[batch_index]) {
                // break if we reach max batch size on any of the batches
                // TODO: just don't add to this batch if it's full
                if batches[batch_index].len() == self.max_batch_size {
                    break;
                }
                batch_index += 1;
            }
        }

        // Send batches to banking threads
        for (batch, sender) in batches
            .into_iter()
            .zip(self.transaction_batch_senders.iter())
        {
            sender.send(batch).unwrap();
        }
    }

    /// Handles packet batches as we receive them from the channel
    fn handle_packet_batches(&mut self, packet_batch_message: PacketBatchMessage) {
        for packet_batch in packet_batch_message {
            let packet_indices: Vec<_> = packet_batch
                .into_iter()
                .enumerate()
                .filter_map(|(idx, p)| if !p.meta.discard() { Some(idx) } else { None })
                .collect();

            self.pending_transactions.extend(
                unprocessed_packet_batches::deserialize_packets(&packet_batch, &packet_indices)
                    .filter_map(|deserialized_packet| {
                        TransactionPriority::try_new(
                            deserialized_packet.immutable_section(),
                            &self.bank,
                        )
                    })
                    .map(|transaction| {
                        if let Ok(account_locks) = transaction
                            .transaction
                            .get_account_locks(&self.bank.feature_set)
                        {
                            // Insert into readonly queues
                            for account in account_locks.readonly {
                                self.transactions_by_account
                                    .entry(*account)
                                    .or_default()
                                    .reads
                                    .insert(transaction.clone());
                            }
                            // Insert into writeonly queues
                            for account in account_locks.writable {
                                self.transactions_by_account
                                    .entry(*account)
                                    .or_default()
                                    .writes
                                    .insert(transaction.clone());
                            }
                        }

                        transaction
                    }),
            );
        }
    }

    /// Handle completed transactions
    fn handle_completed_transaction(&mut self, transaction: TransactionMessage) {
        self.update_queues_on_completed_transaction(&transaction);
        self.push_unblocked_transactions(transaction.transaction.signature());
    }

    /// Sets the bank that we're scheduling for
    fn set_bank(&mut self, bank: Arc<Bank>) {
        self.bank = bank;
    }

    /// Sets the batch size
    fn set_batch_size(&mut self, batch_size: usize) {
        self.max_batch_size = batch_size;
    }

    /// Update account queues on transaction completion
    fn update_queues_on_completed_transaction(&mut self, transaction: &TransactionMessage) {
        // Should always be able to get account locks here since it was a pre-requisite to scheduling
        let account_locks = transaction
            .transaction
            .get_account_locks(&self.bank.feature_set)
            .unwrap();

        for account in account_locks.readonly {
            if self
                .transactions_by_account
                .get_mut(account)
                .unwrap()
                .handle_completed_transaction(&transaction, false)
            {
                self.transactions_by_account.remove(account);
            }
        }

        for account in account_locks.writable {
            if self
                .transactions_by_account
                .get_mut(account)
                .unwrap()
                .handle_completed_transaction(&transaction, true)
            {
                self.transactions_by_account.remove(account);
            }
        }
    }

    /// Check for unblocked transactions on `signature` and push into `pending_transaction`
    fn push_unblocked_transactions(&mut self, signature: &Signature) {
        if let Some(blocked_transactions) = self.blocked_transactions.remove(signature) {
            self.pending_transactions
                .extend(blocked_transactions.into_iter());
        }
    }

    /// Tries to schedule a transaction:
    ///     - If it cannot be scheduled, it is inserted into `blocked_transaction` with the current lowest priority blocking transaction
    ///     - If it can be scheduled, locks are taken, it is pushed into the provided batch.
    fn try_schedule_transaction(
        &mut self,
        transaction: TransactionRef,
        batch: &mut TransactionBatchMessage,
    ) -> bool {
        if let Some(blocking_transaction) =
            self.get_lowest_priority_blocking_transaction(&transaction)
        {
            self.blocked_transactions
                .entry(*blocking_transaction.transaction.signature())
                .or_default()
                .insert(transaction);
            false
        } else {
            self.lock_for_transaction(&transaction);
            batch.push(transaction);
            true
        }
    }

    /// Gets the lowest priority transaction that blocks this one
    fn get_lowest_priority_blocking_transaction(
        &self,
        transaction: &TransactionRef,
    ) -> Option<&TransactionRef> {
        transaction
            .transaction
            .get_account_locks(&self.bank.feature_set)
            .ok()
            .and_then(|account_locks| {
                let min_blocking_transaction = account_locks
                    .readonly
                    .into_iter()
                    .map(|account_key| {
                        self.transactions_by_account
                            .get(account_key)
                            .unwrap()
                            .get_min_blocking_transaction(transaction, false)
                    })
                    .fold(None, option_min);

                account_locks
                    .writable
                    .into_iter()
                    .map(|account_key| {
                        self.transactions_by_account
                            .get(account_key)
                            .unwrap()
                            .get_min_blocking_transaction(transaction, true)
                    })
                    .fold(min_blocking_transaction, option_min)
            })
    }

    /// Apply account locks for a transaction
    fn lock_for_transaction(&mut self, transaction: &TransactionRef) {
        if let Ok(account_locks) = transaction
            .transaction
            .get_account_locks(&self.bank.feature_set)
        {
            for account in account_locks.readonly {
                self.transactions_by_account
                    .get_mut(account)
                    .unwrap()
                    .handle_schedule_transaction(transaction, false);
            }
            for account in account_locks.writable {
                self.transactions_by_account
                    .get_mut(account)
                    .unwrap()
                    .handle_completed_transaction(transaction, true);
            }
        }
    }
}

/// Tracks all pending and blocked transacitons, ordered by priority, for a single account
#[derive(Default)]
struct AccountTransactionQueue {
    /// Tree of read transactions on the account ordered by fee-priority
    reads: BTreeSet<TransactionRef>,
    /// Tree of write transactions on the account ordered by fee-priority
    writes: BTreeSet<TransactionRef>,
    /// Tracks currently scheduled transactions on the account
    scheduled_lock: AccountLock,
}

/// Tracks the currently scheduled lock type and the lowest-fee blocking transaction
#[derive(Debug, Default)]
struct AccountLock {
    lock: AccountLockKind,
    count: usize,
    lowest_priority_transaction: Option<TransactionRef>,
}

#[derive(Debug)]
enum AccountLockKind {
    None,
    Read,
    Write,
}

impl Default for AccountLockKind {
    fn default() -> Self {
        Self::None
    }
}

impl AccountLockKind {
    fn is_none(&self) -> bool {
        match self {
            Self::None => true,
            _ => false,
        }
    }

    fn is_write(&self) -> bool {
        match self {
            Self::Write => true,
            _ => false,
        }
    }

    fn is_read(&self) -> bool {
        match self {
            Self::Read => true,
            _ => false,
        }
    }
}

impl AccountLock {
    fn lock_on_transaction(&mut self, transaction: &TransactionRef, is_write: bool) {
        if is_write {
            assert!(self.lock.is_none()); // no outstanding lock if scheduling a write
            assert!(self.lowest_priority_transaction.is_none());

            self.lock = AccountLockKind::Write;
            self.lowest_priority_transaction = Some(transaction.clone());
        } else {
            assert!(!self.lock.is_write()); // no outstanding write lock if scheduling a read
            self.lock = AccountLockKind::Read;

            match self.lowest_priority_transaction.as_ref() {
                Some(tx) => {
                    if transaction.cmp(tx).is_lt() {
                        self.lowest_priority_transaction = Some(transaction.clone());
                    }
                }
                None => self.lowest_priority_transaction = Some(transaction.clone()),
            }
        }

        self.count += 1;
    }

    fn unlock_on_transaction(&mut self, transaction: &TransactionRef, is_write: bool) {
        assert!(self.lowest_priority_transaction.is_some());
        if is_write {
            assert!(self.lock.is_write());
            assert!(self.count == 1);
        } else {
            assert!(self.lock.is_read());
            assert!(self.count >= 1);
        }

        self.count -= 1;
        if self.count == 0 {
            self.lock = AccountLockKind::None;
            self.lowest_priority_transaction = None;
        }
    }
}

impl AccountTransactionQueue {
    /// Insert a transaction into the queue
    fn insert_transaction(&mut self, transaction: TransactionRef, is_write: bool) {
        if is_write {
            &mut self.writes
        } else {
            &mut self.reads
        }
        .insert(transaction);
    }

    /// Apply account locks for `transaction`
    fn handle_schedule_transaction(&mut self, transaction: &TransactionRef, is_write: bool) {
        self.scheduled_lock
            .lock_on_transaction(transaction, is_write);
    }

    /// Update account queues and lock for completed `transaction`
    ///     Returns true if the account queue can now be cleared
    ///     Returns false if the account queue cannot be cleared
    fn handle_completed_transaction(
        &mut self,
        transaction: &TransactionRef,
        is_write: bool,
    ) -> bool {
        // remove from tree
        if is_write {
            assert!(self.writes.remove(transaction));
        } else {
            assert!(self.reads.remove(transaction));
        }
        // unlock
        self.scheduled_lock
            .unlock_on_transaction(transaction, is_write);

        // Returns true if there are no more transactions in this account queue
        self.writes.len() == 0 && self.reads.len() == 0
    }

    /// Find the minimum-priority transaction that blocks this transaction if there is one
    fn get_min_blocking_transaction<'a>(
        &'a self,
        transaction: &TransactionRef,
        is_write: bool,
    ) -> Option<&'a TransactionRef> {
        let mut min_blocking_transaction = None;
        // Write transactions will be blocked by higher-priority reads, but read transactions will not
        if is_write {
            min_blocking_transaction = option_min(
                min_blocking_transaction,
                upper_bound(&self.reads, transaction.clone()),
            );
        }

        // All transactions are blocked by higher-priority write-transactions
        min_blocking_transaction = option_min(
            min_blocking_transaction,
            upper_bound(&self.writes, transaction.clone()),
        );

        // Schedule write transactions block transactions, regardless of priorty or read/write
        // Scheduled read transactions block write transactions, regardless of priority
        let scheduled_blocking_transaction = if is_write {
            self.scheduled_lock.lowest_priority_transaction.as_ref()
        } else {
            if self.scheduled_lock.lock.is_write() {
                self.scheduled_lock.lowest_priority_transaction.as_ref()
            } else {
                None
            }
        };

        option_min(min_blocking_transaction, scheduled_blocking_transaction)
    }
}

/// Helper function to get the lowest-priority blocking transaction
fn upper_bound<'a>(
    tree: &'a BTreeSet<TransactionRef>,
    transaction: TransactionRef,
) -> Option<&'a TransactionRef> {
    use std::ops::Bound::*;
    let mut iter = tree.range((Excluded(transaction), Unbounded));
    iter.next()
}

/// Helper function to compare options, but None is not considered less than
fn option_min<T: Ord>(lhs: Option<T>, rhs: Option<T>) -> Option<T> {
    match (lhs, rhs) {
        (Some(lhs), Some(rhs)) => Some(std::cmp::min(lhs, rhs)),
        (lhs, None) => lhs,
        (None, rhs) => rhs,
    }
}
