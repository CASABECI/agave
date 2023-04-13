use {
    crate::immutable_deserialized_packet::ImmutableDeserializedPacket,
    solana_sdk::transaction::SanitizedTransaction, std::sync::Arc,
};

/// A unique identifier for a transaction batch.
#[derive(Clone, Copy, Debug, Hash, PartialEq, Eq)]
pub struct TransactionBatchId(u64);

impl TransactionBatchId {
    pub fn new(index: u64) -> Self {
        Self(index)
    }
}

/// A unique identifier for a transaction.
#[derive(Clone, Copy, Debug, Hash, PartialEq, Eq)]
pub struct TransactionId(u64);

impl TransactionId {
    pub fn new(index: u64) -> Self {
        Self(index)
    }
}

/// Message: [Scheduler -> Worker]
/// Transactions to be consumed (executed, recorded, committed)
pub struct ConsumeWork {
    pub batch_id: TransactionBatchId,
    pub transaction_ids: Vec<TransactionId>,
    pub transactions: Vec<SanitizedTransaction>,
}

/// Message: [Worker -> Scheduler]
/// Transactions to be forwarded to the next leader(s)
pub struct ForwardWork {
    pub ids: Vec<TransactionId>,
    pub packets: Vec<Arc<ImmutableDeserializedPacket>>,
}

/// Message: [Worker -> Scheduler]
/// Processed transactions.
pub struct FinishedConsumeWork {
    pub work: ConsumeWork,
    pub retryable_indexes: Vec<usize>,
}

/// Message: [Worker -> Scheduler]
/// Forwarded transactions.
pub struct FinishedForwardWork {
    pub work: ForwardWork,
    pub successful: bool,
}