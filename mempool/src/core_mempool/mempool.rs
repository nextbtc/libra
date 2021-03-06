// Copyright (c) The Libra Core Contributors
// SPDX-License-Identifier: Apache-2.0

//! mempool is used to track transactions which have been submitted but not yet
//! agreed upon.
use crate::{
    core_mempool::{
        index::TxnPointer,
        transaction::{MempoolTransaction, TimelineState},
        transaction_store::TransactionStore,
        ttl_cache::TtlCache,
    },
    OP_COUNTERS,
};
use debug_interface::prelude::*;
use libra_config::config::NodeConfig;
use libra_logger::prelude::*;
use libra_types::{
    account_address::AccountAddress,
    mempool_status::{MempoolStatus, MempoolStatusCode},
    transaction::SignedTransaction,
};
use std::{
    cmp::max,
    collections::HashSet,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

pub struct Mempool {
    // stores metadata of all transactions in mempool (of all states)
    transactions: TransactionStore,

    sequence_number_cache: TtlCache<AccountAddress, u64>,
    // temporary DS. TODO: eventually retire it
    // for each transaction, entry with timestamp is added when transaction enters mempool
    // used to measure e2e latency of transaction in system, as well as time it takes to pick it up
    // by consensus
    pub(crate) metrics_cache: TtlCache<(AccountAddress, u64), SystemTime>,
    pub system_transaction_timeout: Duration,
}

impl Mempool {
    pub fn new(config: &NodeConfig) -> Self {
        Mempool {
            transactions: TransactionStore::new(&config.mempool),
            sequence_number_cache: TtlCache::new(config.mempool.capacity, Duration::from_secs(100)),
            metrics_cache: TtlCache::new(config.mempool.capacity, Duration::from_secs(100)),
            system_transaction_timeout: Duration::from_secs(
                config.mempool.system_transaction_timeout_secs,
            ),
        }
    }

    /// This function will be called once the transaction has been stored
    pub(crate) fn remove_transaction(
        &mut self,
        sender: &AccountAddress,
        sequence_number: u64,
        is_rejected: bool,
    ) {
        trace_event!("mempool:remove_transaction", {"txn", sender, sequence_number});
        trace!(
            "[Mempool] Removing transaction from mempool: {}:{}:{}",
            sender,
            sequence_number,
            is_rejected
        );
        self.log_latency(*sender, sequence_number, "e2e.latency");
        self.metrics_cache.remove(&(*sender, sequence_number));
        OP_COUNTERS.inc(&format!("remove_transaction.{}", is_rejected));

        let current_seq_number = self
            .sequence_number_cache
            .remove(&sender)
            .unwrap_or_default();

        if is_rejected {
            debug!(
                "[Mempool] transaction is rejected: {}:{}",
                sender, sequence_number
            );
            if sequence_number >= current_seq_number {
                self.transactions
                    .reject_transaction(&sender, sequence_number);
            }
        } else {
            // update current cached sequence number for account
            let new_seq_number = max(current_seq_number, sequence_number + 1);
            self.sequence_number_cache.insert(*sender, new_seq_number);
            self.transactions
                .commit_transaction(&sender, new_seq_number);
        }
    }

    fn log_latency(&mut self, account: AccountAddress, sequence_number: u64, metric: &str) {
        if let Some(&creation_time) = self.metrics_cache.get(&(account, sequence_number)) {
            if let Ok(time_delta) = SystemTime::now().duration_since(creation_time) {
                OP_COUNTERS.observe_duration(metric, time_delta);
            }
        }
    }

    /// Used to add a transaction to the Mempool
    /// Performs basic validation: checks account's sequence number
    pub(crate) fn add_txn(
        &mut self,
        txn: SignedTransaction,
        gas_amount: u64,
        rankin_score: u64,
        db_sequence_number: u64,
        timeline_state: TimelineState,
        is_governance_txn: bool,
    ) -> MempoolStatus {
        trace_event!("mempool::add_txn", {"txn", txn.sender(), txn.sequence_number()});
        trace!(
            "[Mempool] Adding transaction to mempool: {}:{}:{}",
            &txn.sender(),
            txn.sequence_number(),
            db_sequence_number,
        );
        let cached_value = self.sequence_number_cache.get(&txn.sender());
        let sequence_number =
            cached_value.map_or(db_sequence_number, |value| max(*value, db_sequence_number));
        self.sequence_number_cache
            .insert(txn.sender(), sequence_number);

        // don't accept old transactions (e.g. seq is less than account's current seq_number)
        if txn.sequence_number() < sequence_number {
            return MempoolStatus::new(MempoolStatusCode::InvalidSeqNumber).with_message(format!(
                "transaction sequence number is {}, current sequence number is  {}",
                txn.sequence_number(),
                sequence_number,
            ));
        }

        let expiration_time = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("init timestamp failure")
            + self.system_transaction_timeout;
        if timeline_state != TimelineState::NonQualified {
            self.metrics_cache
                .insert((txn.sender(), txn.sequence_number()), SystemTime::now());
        }

        let txn_info = MempoolTransaction::new(
            txn,
            expiration_time,
            gas_amount,
            rankin_score,
            timeline_state,
            is_governance_txn,
        );

        let status = self.transactions.insert(txn_info, sequence_number);
        OP_COUNTERS.inc(&format!("insert.{:?}", status));
        status
    }

    /// Fetches next block of transactions for consensus
    /// `batch_size` - size of requested block
    /// `seen_txns` - transactions that were sent to Consensus but were not committed yet
    ///  Mempool should filter out such transactions
    #[allow(clippy::explicit_counter_loop)]
    pub(crate) fn get_block(
        &mut self,
        batch_size: u64,
        mut seen: HashSet<TxnPointer>,
    ) -> Vec<SignedTransaction> {
        let mut result = vec![];
        // Helper DS. Helps to mitigate scenarios where account submits several transactions
        // with increasing gas price (e.g. user submits transactions with sequence number 1, 2
        // and gas_price 1, 10 respectively)
        // Later txn has higher gas price and will be observed first in priority index iterator,
        // but can't be executed before first txn. Once observed, such txn will be saved in
        // `skipped` DS and rechecked once it's ancestor becomes available
        let mut skipped = HashSet::new();
        let seen_size = seen.len();
        let mut txn_walked = 0usize;
        // iterate over the queue of transactions based on gas price
        'main: for txn in self.transactions.iter_queue() {
            txn_walked += 1;
            if seen.contains(&TxnPointer::from(txn)) {
                continue;
            }
            let seq = txn.sequence_number;
            let account_sequence_number = self.sequence_number_cache.get(&txn.address);
            let seen_previous = seq > 0 && seen.contains(&(txn.address, seq - 1));
            // include transaction if it's "next" for given account or
            // we've already sent its ancestor to Consensus
            if seen_previous || account_sequence_number == Some(&seq) {
                let ptr = TxnPointer::from(txn);
                seen.insert(ptr);
                trace_event!("mempool::get_block", {"txn", txn.address, txn.sequence_number});
                result.push(ptr);
                if (result.len() as u64) == batch_size {
                    break;
                }

                // check if we can now include some transactions
                // that were skipped before for given account
                let mut skipped_txn = (txn.address, seq + 1);
                while skipped.contains(&skipped_txn) {
                    seen.insert(skipped_txn);
                    result.push(skipped_txn);
                    if (result.len() as u64) == batch_size {
                        break 'main;
                    }
                    skipped_txn = (txn.address, skipped_txn.1 + 1);
                }
            } else {
                skipped.insert(TxnPointer::from(txn));
            }
        }
        let result_size = result.len();
        // convert transaction pointers to real values
        let block: Vec<_> = result
            .into_iter()
            .filter_map(|(address, seq)| self.transactions.get(&address, seq))
            .collect();
        debug!("mempool::get_block: seen_consensus={}, walked={}, seen_after={}, result_size={}, block_size={}",
               seen_size, txn_walked, seen.len(), result_size, block.len());
        for transaction in &block {
            self.log_latency(
                transaction.sender(),
                transaction.sequence_number(),
                "txn_pre_consensus_s",
            );
        }
        block
    }

    /// periodic core mempool garbage collection
    /// removes all expired transactions
    /// clears expired entries in metrics cache and sequence number cache
    pub(crate) fn gc(&mut self) {
        let now = SystemTime::now();
        self.transactions.gc_by_system_ttl();
        self.metrics_cache.gc(now);
        self.sequence_number_cache.gc(now);
    }

    /// Garbage collection based on client-specified expiration time
    pub(crate) fn gc_by_expiration_time(&mut self, block_time: Duration) {
        self.transactions.gc_by_expiration_time(block_time);
    }

    /// Read `count` transactions from timeline since `timeline_id`
    /// Returns block of transactions and new last_timeline_id
    pub(crate) fn read_timeline(
        &mut self,
        timeline_id: u64,
        count: usize,
    ) -> (Vec<SignedTransaction>, u64) {
        self.transactions.read_timeline(timeline_id, count)
    }

    /// Read transactions from timeline whose timeline id is in range
    /// `start_timeline_id` (exclusive) to `end_timeline_id` (inclusive)
    pub(crate) fn timeline_range(
        &mut self,
        start_timeline_id: u64,
        end_timeline_id: u64,
    ) -> Vec<SignedTransaction> {
        self.transactions
            .timeline_range(start_timeline_id, end_timeline_id)
    }
}
