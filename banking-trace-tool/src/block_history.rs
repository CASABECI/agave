//! Get signatures from historical blocks. Used to filter out transactions from other logs.
//!

use {
    itertools::Itertools,
    solana_rpc_client::rpc_client::RpcClient,
    solana_rpc_client_api::config::RpcBlockConfig,
    solana_sdk::{
        clock::{Slot, MAX_PROCESSING_AGE},
        signature::Signature,
    },
    solana_transaction_status::{TransactionDetails, UiConfirmedBlock},
    std::collections::{HashMap, HashSet},
};

#[derive(Default)]
pub struct HistoryChecker {
    blockhashes: HashSet<String>,
    signatures: HashSet<Signature>,
    actual_signatures: HashMap<Signature, usize>, // signature to index in block
}

pub enum FilterKind {
    MissingBlockhash,
    AlreadyProcessed,
}

impl HistoryChecker {
    /// Returns `Some(FilterKind)` if a transaction does not meet historical requirements:
    /// 1. blockhash does not appear recently
    /// 2. transaction signature already appears in a block
    pub fn should_filter(&self, recent_blockhash: &String, sig: &Signature) -> Option<FilterKind> {
        if !self.blockhashes.contains(recent_blockhash) {
            Some(FilterKind::MissingBlockhash)
        } else if self.signatures.contains(sig) {
            Some(FilterKind::AlreadyProcessed)
        } else {
            None
        }
    }

    /// Check if the slot actually contained a signature.
    pub fn actually_contained(&self, sig: &Signature) -> Option<usize> {
        self.actual_signatures.get(sig).copied()
    }
}

/// Save the history of transaction signatures and recent blockhashes before `slot`.
pub fn save_history_before(slot: Slot) {
    let file = format!("./{}.recent_history", slot);
    // retrieve history
    let client = RpcClient::new("https://api.mainnet-beta.solana.com");
    let mut blockhashes = Vec::with_capacity(MAX_PROCESSING_AGE);
    let mut signatures = Vec::new();
    for slot in (0u64..slot).rev().take(MAX_PROCESSING_AGE) {
        println!("checking slot {slot}");
        let Some(block) = get_block(&client, slot) else {
            continue;
        };
        blockhashes.push(block.blockhash);
        signatures.extend(
            block
                .transactions
                .unwrap()
                .iter()
                .map(|tx| tx.transaction.decode().unwrap().signatures[0]),
        );
    }

    let actual_block = get_block(&client, slot).unwrap();
    let actual_signatures = actual_block
        .transactions
        .unwrap()
        .iter()
        .enumerate()
        .map(|(index, tx)| (tx.transaction.decode().unwrap().signatures[0], index))
        .collect_vec();

    // Save to file
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .write(true)
        .open(file)
        .unwrap();

    bincode::serialize_into(&mut file, &blockhashes).unwrap();
    bincode::serialize_into(&mut file, &signatures).unwrap();
    bincode::serialize_into(&mut file, &actual_signatures).unwrap();
}

pub fn get_block(client: &RpcClient, slot: Slot) -> Option<UiConfirmedBlock> {
    client
        .get_block_with_config(
            slot,
            RpcBlockConfig {
                encoding: Some(solana_transaction_status::UiTransactionEncoding::Base58),
                transaction_details: Some(TransactionDetails::Full),
                rewards: None,
                commitment: Some(solana_sdk::commitment_config::CommitmentConfig {
                    commitment: solana_sdk::commitment_config::CommitmentLevel::Finalized,
                }),
                max_supported_transaction_version: Some(0),
            },
        )
        .ok()
}

/// Load the history of transaction signatures and recent blockhashes from file.
pub fn load_history(slot: Slot) -> HistoryChecker {
    let file = format!("./{}.recent_history", slot);
    let mut file = std::fs::OpenOptions::new().read(true).open(file).unwrap();

    let blockhashes: Vec<String> = bincode::deserialize_from(&mut file).unwrap();
    let signatures: Vec<Signature> = bincode::deserialize_from(&mut file).unwrap();
    let actual_signatures: Vec<(Signature, usize)> = bincode::deserialize_from(&mut file).unwrap();

    HistoryChecker {
        blockhashes: blockhashes.into_iter().collect(),
        signatures: signatures.into_iter().collect(),
        actual_signatures: actual_signatures.into_iter().collect(),
    }
}
