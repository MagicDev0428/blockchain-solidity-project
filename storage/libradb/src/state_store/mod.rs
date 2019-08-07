// Copyright (c) The Libra Core Contributors
// SPDX-License-Identifier: Apache-2.0

//! This file defines state store APIs that are related account state Merkle tree.

#[cfg(test)]
mod state_store_test;

use crate::{
    change_set::ChangeSet,
    ledger_counters::LedgerCounter,
    schema::{
        jellyfish_merkle_node::JellyfishMerkleNodeSchema,
        retired_state_record::StaleNodeIndexSchema,
    },
};
use crypto::{hash::CryptoHash, HashValue};
use failure::prelude::*;
use jellyfish_merkle::{
    node_type::{Node, NodeKey},
    JellyfishMerkleTree, TreeReader,
};
use schemadb::DB;
use std::{collections::HashMap, sync::Arc};
use types::{
    account_address::AccountAddress, account_state_blob::AccountStateBlob,
    proof::SparseMerkleProof, transaction::Version,
};

pub(crate) struct StateStore {
    db: Arc<DB>,
}

impl StateStore {
    pub fn new(db: Arc<DB>) -> Self {
        Self { db }
    }

    /// Get the account state blob given account address and root hash of state Merkle tree
    pub fn get_account_state_with_proof_by_version(
        &self,
        address: AccountAddress,
        version: Version,
    ) -> Result<(Option<AccountStateBlob>, SparseMerkleProof)> {
        let (blob, proof) =
            JellyfishMerkleTree::new(self).get_with_proof(address.hash(), version)?;
        Ok((blob, proof))
    }

    /// Put the results generated by `account_state_sets` to `batch` and return the result root
    /// hashes for each write set.
    pub fn put_account_state_sets(
        &self,
        account_state_sets: Vec<HashMap<AccountAddress, AccountStateBlob>>,
        first_version: Version,
        cs: &mut ChangeSet,
    ) -> Result<Vec<HashValue>> {
        let blob_sets = account_state_sets
            .into_iter()
            .map(|account_states| {
                account_states
                    .into_iter()
                    .map(|(addr, blob)| (addr.hash(), blob))
                    .collect::<Vec<_>>()
            })
            .collect::<Vec<_>>();

        let (new_root_hash_vec, tree_update_batch) =
            JellyfishMerkleTree::new(self).put_blob_sets(blob_sets, first_version)?;

        cs.counter_bumps.bump(
            LedgerCounter::StateNodesCreated,
            tree_update_batch.node_batch.len(),
        );
        cs.counter_bumps.bump(
            LedgerCounter::StateBlobsCreated,
            tree_update_batch.num_new_leaves,
        );
        tree_update_batch
            .node_batch
            .iter()
            .map(|(node_key, node)| cs.batch.put::<JellyfishMerkleNodeSchema>(node_key, node))
            .collect::<Result<Vec<()>>>()?;

        cs.counter_bumps.bump(
            LedgerCounter::StateNodesRetired,
            tree_update_batch.stale_node_index_batch.len(),
        );
        cs.counter_bumps.bump(
            LedgerCounter::StateBlobsRetired,
            tree_update_batch.num_stale_leaves,
        );
        tree_update_batch
            .stale_node_index_batch
            .iter()
            .map(|row| cs.batch.put::<StaleNodeIndexSchema>(row, &()))
            .collect::<Result<Vec<()>>>()?;

        Ok(new_root_hash_vec)
    }
}

impl TreeReader for StateStore {
    fn get_node(&self, node_key: &NodeKey) -> Result<Node> {
        Ok(self
            .db
            .get::<JellyfishMerkleNodeSchema>(node_key)?
            .ok_or_else(|| format_err!("Failed to find node with node_key {:?}", node_key))?)
    }
}
