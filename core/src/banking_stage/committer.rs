use {
    super::leader_slot_timing_metrics::LeaderExecuteAndCommitTimings,
    itertools::Itertools,
    solana_ledger::blockstore_processor::TransactionStatusSender,
    solana_measure::measure_us,
    solana_runtime::{
        bank::{Bank, CommitTransactionCounts},
        bank_utils,
        prioritization_fee_cache::PrioritizationFeeCache,
    },
    solana_sdk::{hash::Hash, pubkey::Pubkey},
    solana_signed_message::SignedMessage,
    solana_svm::{
        account_loader::TransactionLoadResult,
        transaction_results::{TransactionExecutionResult, TransactionResults},
    },
    solana_transaction_status::TransactionTokenBalance,
    solana_vote::vote_sender_types::ReplayVoteSender,
    std::{collections::HashMap, sync::Arc},
};

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CommitTransactionDetails {
    Committed { compute_units: u64 },
    NotCommitted,
}

#[derive(Default)]
pub(super) struct PreBalanceInfo {
    pub native: Vec<Vec<u64>>,
    pub token: Vec<Vec<TransactionTokenBalance>>,
    pub mint_decimals: HashMap<Pubkey, u8>,
}

#[derive(Clone)]
pub struct Committer {
    transaction_status_sender: Option<TransactionStatusSender>,
    replay_vote_sender: ReplayVoteSender,
    prioritization_fee_cache: Arc<PrioritizationFeeCache>,
}

impl Committer {
    pub fn new(
        transaction_status_sender: Option<TransactionStatusSender>,
        replay_vote_sender: ReplayVoteSender,
        prioritization_fee_cache: Arc<PrioritizationFeeCache>,
    ) -> Self {
        Self {
            transaction_status_sender,
            replay_vote_sender,
            prioritization_fee_cache,
        }
    }

    pub(super) fn transaction_status_sender_enabled(&self) -> bool {
        self.transaction_status_sender.is_some()
    }

    #[allow(clippy::too_many_arguments)]
    pub(super) fn commit_transactions(
        &self,
        txs: &[impl SignedMessage],
        loaded_transactions: &mut [TransactionLoadResult],
        execution_results: Vec<TransactionExecutionResult>,
        last_blockhash: Hash,
        lamports_per_signature: u64,
        starting_transaction_index: Option<usize>,
        bank: &Arc<Bank>,
        pre_balance_info: &mut PreBalanceInfo,
        execute_and_commit_timings: &mut LeaderExecuteAndCommitTimings,
        signature_count: u64,
        executed_transactions_count: usize,
        executed_non_vote_transactions_count: usize,
        executed_with_successful_result_count: usize,
    ) -> (u64, Vec<CommitTransactionDetails>) {
        let executed_transactions = execution_results
            .iter()
            .zip(txs)
            .filter_map(|(execution_result, tx)| execution_result.was_executed().then_some(tx))
            .collect_vec();

        let (tx_results, commit_time_us) = measure_us!(bank.commit_transactions(
            txs,
            loaded_transactions,
            execution_results,
            last_blockhash,
            lamports_per_signature,
            CommitTransactionCounts {
                committed_transactions_count: executed_transactions_count as u64,
                committed_non_vote_transactions_count: executed_non_vote_transactions_count as u64,
                committed_with_failure_result_count: executed_transactions_count
                    .saturating_sub(executed_with_successful_result_count)
                    as u64,
                signature_count,
            },
            &mut execute_and_commit_timings.execute_timings,
        ));
        execute_and_commit_timings.commit_us = commit_time_us;

        let commit_transaction_statuses = tx_results
            .execution_results
            .iter()
            .map(|execution_result| match execution_result.details() {
                Some(details) => CommitTransactionDetails::Committed {
                    compute_units: details.executed_units,
                },
                None => CommitTransactionDetails::NotCommitted,
            })
            .collect();

        let ((), find_and_send_votes_us) = measure_us!({
            bank_utils::find_and_send_votes(txs, &tx_results, Some(&self.replay_vote_sender));
            self.collect_balances_and_send_status_batch(
                tx_results,
                bank,
                txs,
                pre_balance_info,
                starting_transaction_index,
            );
            self.prioritization_fee_cache
                .update(bank, executed_transactions.into_iter());
        });
        execute_and_commit_timings.find_and_send_votes_us = find_and_send_votes_us;
        (commit_time_us, commit_transaction_statuses)
    }

    fn collect_balances_and_send_status_batch(
        &self,
        _tx_results: TransactionResults,
        _bank: &Arc<Bank>,
        _txs: &[impl SignedMessage],
        _pre_balance_info: &mut PreBalanceInfo,
        _starting_transaction_index: Option<usize>,
    ) {
        if let Some(_transaction_status_sender) = &self.transaction_status_sender {
            // TODO: Implement this for generic SignedMessage type.
            // Skipping for now since this is RPC only.
            todo!("implement transaction status sending")
            // let txs = batch.sanitized_transactions().to_vec();
            // let post_balances = bank.collect_balances(txs);
            // let post_token_balances =
            //     collect_token_balances(bank, txs, &mut pre_balance_info.mint_decimals);
            // let mut transaction_index = starting_transaction_index.unwrap_or_default();
            // let batch_transaction_indexes: Vec<_> = tx_results
            //     .execution_results
            //     .iter()
            //     .map(|result| {
            //         if result.was_executed() {
            //             let this_transaction_index = transaction_index;
            //             saturating_add_assign!(transaction_index, 1);
            //             this_transaction_index
            //         } else {
            //             0
            //         }
            //     })
            //     .collect();
            // transaction_status_sender.send_transaction_status_batch(
            //     bank.clone(),
            //     txs,
            //     tx_results.execution_results,
            //     TransactionBalancesSet::new(
            //         std::mem::take(&mut pre_balance_info.native),
            //         post_balances,
            //     ),
            //     TransactionTokenBalancesSet::new(
            //         std::mem::take(&mut pre_balance_info.token),
            //         post_token_balances,
            //     ),
            //     tx_results.rent_debits,
            //     batch_transaction_indexes,
            // );
        }
    }
}
