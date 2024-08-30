use {
    solana_sdk::transaction::{SanitizedTransaction, VersionedTransaction},
    solana_svm_transaction::svm_transaction::SVMTransaction,
};

/// This trait is intended to be temporary.
/// It is required now because certain interfaces in `runtime` and `core`
/// currently force the use of old/sdk transaction types.
/// During the transition to a new transaction type, these interfaces will
/// not yet be updated to have a more generic interface.
/// As such this trait is used to bridge the gap between the old and new
/// transaction types - the main processing pipelines can be updated
/// to be generic over a transaction interface while still allowing the
/// necessary conversions when the interfaces forcing old transaction types
/// are invoked.
pub trait SVMTransactionAdapter: SVMTransaction {
    /// Convert to a `SanitizedTransaction`.
    fn to_sanitized_transaction(&self) -> SanitizedTransaction;
    /// Convert to a `VersionedTransaction`.
    fn to_versioned_transaction(&self) -> VersionedTransaction;
}

impl SVMTransactionAdapter for SanitizedTransaction {
    fn to_sanitized_transaction(&self) -> SanitizedTransaction {
        self.clone()
    }

    fn to_versioned_transaction(&self) -> VersionedTransaction {
        self.to_versioned_transaction()
    }
}
