// Copyright (c) The Diem Core Contributors
// SPDX-License-Identifier: Apache-2.0

use std::sync::Arc;

use futures::{
    channel::{
        mpsc::{UnboundedReceiver, UnboundedSender},
        oneshot,
    },
    SinkExt, StreamExt,
};
use tokio::time::Duration;

use consensus_types::{common::Author, executed_block::ExecutedBlock};
use diem_logger::prelude::*;
use diem_types::{
    account_address::AccountAddress,
    ledger_info::{LedgerInfo, LedgerInfoWithSignatures},
    validator_verifier::ValidatorVerifier,
};

use crate::{
    experimental::{
        buffer_item::BufferItem,
        execution_phase::{ExecutionRequest, ExecutionResponse},
        linkedlist::{find_elem, get_elem, get_next, link_eq, set_elem, take_elem, Link, List},
        persisting_phase::PersistingRequest,
        signing_phase::{SigningRequest, SigningResponse},
    },
    network::NetworkSender,
    network_interface::ConsensusMsg,
    round_manager::VerifiedEvent,
    state_replication::StateComputerCommitCallBackType,
};
use futures::executor::block_on;
use std::ops::Deref;

pub const BUFFER_MANAGER_RETRY_INTERVAL: u64 = 1000;

pub type SyncAck = ();

pub fn sync_ack_new() -> SyncAck {}

pub struct SyncRequest {
    tx: oneshot::Sender<SyncAck>,
    ledger_info: LedgerInfoWithSignatures,
    reconfig: bool,
}

pub struct OrderedBlocks {
    pub ordered_blocks: Vec<ExecutedBlock>,
    pub ordered_proof: LedgerInfoWithSignatures,
    pub callback: StateComputerCommitCallBackType,
}

pub type BufferItemRootType = Link<BufferItem>;
pub type Sender<T> = UnboundedSender<T>;
pub type Receiver<T> = UnboundedReceiver<T>;

/// StateManager handles the states of ordered blocks and
/// interacts with the execution phase, the signing phase, and
/// the persisting phase.
pub struct StateManager {
    author: Author,

    buffer: List<BufferItem>,

    // the roots point to the first *unprocessed* item.
    // None means no items ready to be processed (either all processed or no item finishes previous stage)
    execution_root: BufferItemRootType,
    execution_phase_tx: Sender<ExecutionRequest>,
    execution_phase_rx: Receiver<ExecutionResponse>,

    signing_root: BufferItemRootType,
    signing_phase_tx: Sender<SigningRequest>,
    signing_phase_rx: Receiver<SigningResponse>,

    commit_msg_tx: NetworkSender,
    commit_msg_rx: channel::diem_channel::Receiver<AccountAddress, VerifiedEvent>,

    // we don't hear back from the persisting phase
    persisting_phase_tx: Sender<PersistingRequest>,

    block_rx: UnboundedReceiver<OrderedBlocks>,
    sync_rx: UnboundedReceiver<SyncRequest>,
    end_epoch: bool,

    verifier: ValidatorVerifier,
}

impl StateManager {
    pub fn new(
        author: Author,
        execution_phase_tx: Sender<ExecutionRequest>,
        execution_phase_rx: Receiver<ExecutionResponse>,
        signing_phase_tx: Sender<SigningRequest>,
        signing_phase_rx: Receiver<SigningResponse>,
        commit_msg_tx: NetworkSender,
        commit_msg_rx: channel::diem_channel::Receiver<AccountAddress, VerifiedEvent>,
        persisting_phase_tx: Sender<PersistingRequest>,
        block_rx: UnboundedReceiver<OrderedBlocks>,
        sync_rx: UnboundedReceiver<SyncRequest>,
        verifier: ValidatorVerifier,
    ) -> Self {
        let buffer = List::<BufferItem>::new();

        // point the roots to the head
        let execution_root = buffer.head.as_ref().cloned();
        let signing_root = buffer.head.as_ref().cloned();

        Self {
            author,

            buffer,

            execution_root,
            execution_phase_tx,
            execution_phase_rx,

            signing_root,
            signing_phase_tx,
            signing_phase_rx,

            commit_msg_tx,
            commit_msg_rx,

            persisting_phase_tx,

            block_rx,
            sync_rx,
            end_epoch: false,

            verifier,
        }
    }

    /// process incoming ordered blocks
    /// push them into the buffer and update the roots if they are none.
    fn process_ordered_blocks(&mut self, ordered_blocks: OrderedBlocks) {
        let OrderedBlocks {
            ordered_blocks,
            ordered_proof,
            callback,
        } = ordered_blocks;

        let item = BufferItem::new_ordered(ordered_blocks.clone(), ordered_proof, callback);
        // push blocks to buffer
        self.buffer.push_back(item);
    }

    /// Set the execution root to the first not executed item (Ordered) and send execution request
    /// Set to None if not exist
    async fn advance_execution_root(&mut self) {
        let cursor = self.execution_root.clone().or(self.buffer.head.clone());
        self.execution_root = find_elem(cursor, |item| item.is_ordered());
        if self.execution_root.is_some() {
            let ordered_blocks = get_elem(&self.execution_root).get_blocks().clone();
            self.execution_phase_tx
                .send(ExecutionRequest { ordered_blocks })
                .await
                .expect("Failed to send execution request")
        }
    }

    /// Set the signing root to the first not signed item (Executed) and send execution request
    /// Set to None if not exist
    async fn advance_signing_root(&mut self) {
        let cursor = self.signing_root.clone().or(self.buffer.head.clone());
        self.signing_root = find_elem(cursor, |item| item.is_executed());
        if self.signing_root.is_some() {
            let item = get_elem(&self.signing_root);
            match item.deref() {
                BufferItem::Executed(executed_item) => {
                    let commit_ledger_info = LedgerInfo::new(
                        executed_item.executed_blocks.last().unwrap().block_info(),
                        executed_item
                            .ordered_proof
                            .ledger_info()
                            .consensus_data_hash(),
                    );
                    self.signing_phase_tx
                        .send(SigningRequest {
                            ordered_ledger_info: executed_item.ordered_proof.clone(),
                            commit_ledger_info,
                        })
                        .await
                        .expect("Failed to send signing request");
                }
                _ => unreachable!(),
            }
        }
    }

    /// Pop the prefix of buffer items until (including) aggregated_item_cursor
    /// Send persist request.
    fn advance_head(&mut self, aggregated_cursor: BufferItemRootType) {
        let target_block_id = {
            let item = get_elem(&aggregated_cursor);
            assert!(item.is_aggregated());
            item.block_id()
        };
        let mut blocks_to_persist: Vec<Arc<ExecutedBlock>> = vec![];

        while let Some(item) = self.buffer.pop_front() {
            blocks_to_persist.extend(
                item.get_blocks()
                    .iter()
                    .map(|eb| Arc::new(eb.clone()))
                    .collect::<Vec<Arc<ExecutedBlock>>>(),
            );
            if item.block_id() == target_block_id {
                if let BufferItem::Aggregated(aggregated) = item {
                    block_on(self.persisting_phase_tx.send(PersistingRequest {
                        blocks: blocks_to_persist,
                        commit_ledger_info: aggregated.aggregated_proof,
                        // we use the last callback
                        // this is okay because the callback function (from BlockStore::commit)
                        // takes in the actual blocks and ledger info from the state computer
                        // the encoded values are references to the block_tree, storage, and a commit root
                        // the block_tree and storage are the same for all the callbacks in the current epoch
                        // the commit root is used in logging only.
                        callback: aggregated.callback,
                    }));
                    return;
                } else {
                    unreachable!("Expect aggregated item");
                }
            }
        }
        unreachable!("Aggregated item not found in the list");
    }

    /// update the root to None;
    fn reset_all_roots(&mut self) {
        self.signing_root = None;
        self.execution_root = None;
    }

    /// this function processes a sync request
    /// if reconfig flag is set, it stops the main loop
    /// otherwise, it looks for a matching buffer item.
    /// If found and the item is executed/signed, advance it to aggregated and try_persisting
    /// Otherwise, it adds the signature to cache, later it will get advanced to aggregated
    /// finally, it sends back an ack.
    async fn process_sync_request(&mut self, sync_event: SyncRequest) {
        let SyncRequest {
            tx,
            ledger_info,
            reconfig,
        } = sync_event;

        if reconfig {
            // buffer manager will stop
            self.end_epoch = true;
        } else {
            // look for the target ledger info:
            // if found: we try to advance it to aggregated, if succeeded, we try persisting the items.
            // if not found: it means the block is in BlockStore but not in the buffer
            // either the block is just persisted, or has not been added to the buffer
            // in either cases, we do nothing.
            let cursor = find_elem(self.buffer.head.clone(), |item| {
                item.block_id() == ledger_info.commit_info().id()
            });
            if cursor.is_some() {
                let buffer_item = take_elem(&cursor);
                let attempted_item =
                    buffer_item.try_advance_to_aggregated_with_ledger_info(ledger_info.clone());
                let aggregated = attempted_item.is_aggregated();
                set_elem(&cursor, attempted_item);
                if aggregated {
                    self.advance_head(cursor);
                }
            }

            // reset roots because the item pointed by them might no longer exist
            self.reset_all_roots();
        }

        // ack reset
        tx.send(sync_ack_new()).unwrap();
    }

    /// If the response is successful, advance the item to Executed, otherwise panic (TODO fix).
    async fn process_execution_response(&mut self, response: ExecutionResponse) {
        let ExecutionResponse { inner } = response;
        let executed_blocks = inner.expect("Execution failure");

        // find the corresponding item, may not exist if a reset or aggregated happened
        let current_cursor = find_elem(self.execution_root.clone(), |item| {
            item.block_id() == executed_blocks.last().unwrap().id()
        });

        if current_cursor.is_some() {
            let buffer_item = take_elem(&current_cursor);
            assert!(buffer_item.is_ordered());
            set_elem(
                &current_cursor,
                buffer_item.advance_to_executed(executed_blocks),
            );
        }
    }

    /// If the signing response is successful, advance the item to Signed and broadcast commit votes.
    async fn process_signing_response(&mut self, response: SigningResponse) {
        let SigningResponse {
            signature_result,
            commit_ledger_info,
        } = response;
        let signature = match signature_result {
            Ok(sig) => sig,
            Err(e) => {
                error!("Signing failed {:?}", e);
                return;
            }
        };
        // find the corresponding item, may not exist if a reset or aggregated happened
        let current_cursor = find_elem(self.signing_root.clone(), |item| {
            item.block_id() == commit_ledger_info.commit_info().id()
        });
        if current_cursor.is_some() {
            let buffer_item = take_elem(&current_cursor);
            // it is possible that we already signed this buffer item (double check after the final integration)
            if buffer_item.is_executed() {
                // we have found the buffer item
                let (signed_buffer_item, commit_vote) =
                    buffer_item.advance_to_signed(self.author, signature, &self.verifier);

                set_elem(&current_cursor, signed_buffer_item);

                self.commit_msg_tx
                    .broadcast(ConsensusMsg::CommitVoteMsg(Box::new(commit_vote)))
                    .await;
            }
        }
    }

    /// process the commit vote messages
    /// it scans the whole buffer for a matching blockinfo
    /// if found, try advancing the item to be aggregated
    async fn process_commit_message(
        &mut self,
        commit_msg: VerifiedEvent,
    ) -> Option<BufferItemRootType> {
        match commit_msg {
            VerifiedEvent::CommitVote(vote) => {
                // find the corresponding item
                let current_cursor = find_elem(self.buffer.head.clone(), |item| {
                    item.block_id() == vote.commit_info().id()
                });
                if current_cursor.is_some() {
                    let mut buffer_item = take_elem(&current_cursor);
                    let new_item = match buffer_item.add_signature_if_matched(
                        vote.commit_info(),
                        vote.author(),
                        vote.signature().clone(),
                    ) {
                        Ok(()) => buffer_item.try_advance_to_aggregated(&self.verifier),
                        Err(e) => {
                            error!("Failed to add commit vote {:?}", e);
                            buffer_item
                        }
                    };
                    set_elem(&current_cursor, new_item);
                    if get_elem(&current_cursor).is_aggregated() {
                        return Some(current_cursor);
                    }
                }
            }
            _ => {
                unreachable!();
            }
        }
        None
    }

    /// this function retries all the items until the signing root
    /// note that there might be other signed items after the signing root
    async fn retry_broadcasting_commit_votes(&mut self) {
        let mut cursor = self.buffer.head.clone();
        while cursor.is_some() && !link_eq(&cursor, &self.signing_root) {
            // we move forward before sending the message
            // just in case the buffer becomes empty during await.
            let next_cursor = get_next(&cursor);
            {
                let buffer_item = get_elem(&cursor);
                match buffer_item.deref() {
                    BufferItem::Aggregated(_) => continue, // skip aggregated items
                    BufferItem::Signed(signed) => {
                        self.commit_msg_tx
                            .broadcast(ConsensusMsg::CommitVoteMsg(Box::new(
                                signed.commit_vote.clone(),
                            )))
                            .await;
                    }
                    _ => {
                        unreachable!()
                    }
                }
            }
            cursor = next_cursor;
        }
    }

    async fn start(mut self) {
        info!("Buffer manager starts.");
        let mut interval =
            tokio::time::interval(Duration::from_millis(BUFFER_MANAGER_RETRY_INTERVAL));
        while !self.end_epoch {
            // advancing the root will trigger sending requests to the pipeline
            tokio::select! {
                Some(blocks) = self.block_rx.next() => {
                    self.process_ordered_blocks(blocks);
                    if self.execution_root.is_none() {
                        self.advance_execution_root().await;
                    }
                }
                Some(reset_event) = self.sync_rx.next() => {
                    self.process_sync_request(reset_event).await;
                    if self.execution_root.is_none() {
                        self.advance_execution_root().await;
                    }
                    if self.signing_root.is_none() {
                        self.advance_signing_root().await;
                    }
                }
                Some(response) = self.execution_phase_rx.next() => {
                    self.process_execution_response(response).await;
                    self.advance_execution_root().await;
                    if self.signing_root.is_none() {
                        self.advance_signing_root().await;
                    }
                }
                Some(response) = self.signing_phase_rx.next() => {
                    self.process_signing_response(response).await;
                    self.advance_signing_root().await;
                }
                Some(commit_msg) = self.commit_msg_rx.next() => {
                    if let Some(aggregated) = self.process_commit_message(commit_msg).await {
                        self.advance_head(aggregated);
                    }
                }
                _ = interval.tick() => {
                    self.retry_broadcasting_commit_votes().await;
                }
                // no else branch here because interval.tick will always be available
            }
        }
    }
}
