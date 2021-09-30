// Copyright (c) The Diem Core Contributors
// SPDX-License-Identifier: Apache-2.0

#![forbid(unsafe_code)]

use diem_crypto::{hash::EventAccumulatorHasher, HashValue};
use diem_types::{
    account_address::AccountAddress,
    account_state_blob::AccountStateBlob,
    contract_event::ContractEvent,
    epoch_state::EpochState,
    nibble::nibble_path::NibblePath,
    on_chain_config,
    proof::accumulator::InMemoryAccumulator,
    transaction::{TransactionStatus, Version},
    write_set::WriteSet,
};
use executor_types::{ExecutedTrees, StateComputeResult};
use std::{collections::HashMap, sync::Arc};

/// The entire set of data associated with a transaction. In addition to the output generated by VM
/// which includes the write set and events, this also has the in-memory trees.
#[derive(Clone, Debug)]
pub struct TransactionData {
    /// Each entry in this map represents the new blob value of an account touched by this
    /// transaction. The blob is obtained by deserializing the previous blob into a BTreeMap,
    /// applying relevant portion of write set on the map and serializing the updated map into a
    /// new blob.
    account_blobs: HashMap<AccountAddress, AccountStateBlob>,

    /// Each entry in this map represents the the hash of a newly generated jellyfish node
    /// and its corresponding nibble path.
    jf_node_hashes: HashMap<NibblePath, HashValue>,

    /// The writeset generated from this transaction.
    write_set: WriteSet,

    /// The list of events emitted during this transaction.
    events: Vec<ContractEvent>,

    /// The execution status set by the VM.
    status: TransactionStatus,

    /// Root hash of the state tree.
    state_root_hash: HashValue,

    /// The in-memory Merkle Accumulator that has all events emitted by this transaction.
    event_tree: Arc<InMemoryAccumulator<EventAccumulatorHasher>>,

    /// The amount of gas used.
    gas_used: u64,

    /// The transaction info hash if the VM status output was keep, None otherwise
    txn_info_hash: Option<HashValue>,
}

impl TransactionData {
    pub fn new(
        account_blobs: HashMap<AccountAddress, AccountStateBlob>,
        jf_node_hashes: HashMap<NibblePath, HashValue>,
        write_set: WriteSet,
        events: Vec<ContractEvent>,
        status: TransactionStatus,
        state_root_hash: HashValue,
        event_tree: Arc<InMemoryAccumulator<EventAccumulatorHasher>>,
        gas_used: u64,
        txn_info_hash: Option<HashValue>,
    ) -> Self {
        TransactionData {
            account_blobs,
            jf_node_hashes,
            write_set,
            events,
            status,
            state_root_hash,
            event_tree,
            gas_used,
            txn_info_hash,
        }
    }

    pub fn account_blobs(&self) -> &HashMap<AccountAddress, AccountStateBlob> {
        &self.account_blobs
    }

    pub fn jf_node_hashes(&self) -> &HashMap<NibblePath, HashValue> {
        &self.jf_node_hashes
    }

    pub fn write_set(&self) -> &WriteSet {
        &self.write_set
    }

    pub fn events(&self) -> &[ContractEvent] {
        &self.events
    }

    pub fn status(&self) -> &TransactionStatus {
        &self.status
    }

    pub fn state_root_hash(&self) -> HashValue {
        self.state_root_hash
    }

    pub fn event_root_hash(&self) -> HashValue {
        self.event_tree.root_hash()
    }

    pub fn gas_used(&self) -> u64 {
        self.gas_used
    }

    pub fn txn_info_hash(&self) -> Option<HashValue> {
        self.txn_info_hash
    }
}

/// The output of Processing the vm output of a series of transactions to the parent
/// in-memory state merkle tree and accumulator.
#[derive(Debug, Clone)]
pub struct ProcessedVMOutput {
    /// The entire set of data associated with each transaction.
    transaction_data: Vec<TransactionData>,

    /// The in-memory Merkle Accumulator and state Sparse Merkle Tree after appending all the
    /// transactions in this set.
    executed_trees: ExecutedTrees,

    /// If set, this is the new epoch info that should be changed to if this block is committed.
    epoch_state: Option<EpochState>,
}

impl ProcessedVMOutput {
    pub fn new(
        transaction_data: Vec<TransactionData>,
        executed_trees: ExecutedTrees,
        epoch_state: Option<EpochState>,
    ) -> Self {
        ProcessedVMOutput {
            transaction_data,
            executed_trees,
            epoch_state,
        }
    }

    pub fn transaction_data(&self) -> &[TransactionData] {
        &self.transaction_data
    }

    pub fn executed_trees(&self) -> &ExecutedTrees {
        &self.executed_trees
    }

    pub fn accu_root(&self) -> HashValue {
        self.executed_trees().state_id()
    }

    pub fn version(&self) -> Option<Version> {
        self.executed_trees().version()
    }

    pub fn epoch_state(&self) -> &Option<EpochState> {
        &self.epoch_state
    }

    pub fn has_reconfiguration(&self) -> bool {
        self.epoch_state.is_some()
    }

    pub fn compute_result(
        &self,
        parent_frozen_subtree_roots: Vec<HashValue>,
        parent_num_leaves: u64,
    ) -> StateComputeResult {
        let new_epoch_event_key = on_chain_config::new_epoch_event_key();
        let txn_accu = self.executed_trees().txn_accumulator();

        let mut compute_status = Vec::new();
        let mut transaction_info_hashes = Vec::new();
        let mut reconfig_events = Vec::new();

        for txn_data in self.transaction_data() {
            let status = txn_data.status();
            compute_status.push(status.clone());
            if matches!(status, TransactionStatus::Keep(_)) {
                transaction_info_hashes.push(txn_data.txn_info_hash().expect("Txn to be kept."));
                reconfig_events.extend(
                    txn_data
                        .events()
                        .iter()
                        .filter(|e| *e.key() == new_epoch_event_key)
                        .cloned(),
                )
            }
        }

        // Now that we have the root hash and execution status we can send the response to
        // consensus.
        // TODO: The VM will support a special transaction to set the validators for the
        // next epoch that is part of a block execution.
        StateComputeResult::new(
            self.accu_root(),
            txn_accu.frozen_subtree_roots().clone(),
            txn_accu.num_leaves(),
            parent_frozen_subtree_roots,
            parent_num_leaves,
            self.epoch_state.clone(),
            compute_status,
            transaction_info_hashes,
            reconfig_events,
        )
    }
}
