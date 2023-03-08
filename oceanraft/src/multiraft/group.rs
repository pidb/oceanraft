use std::collections::HashMap;

use prost::Message;
use raft::prelude::Entry;
use raft::LightReady;
use raft::RawNode;
use raft::ReadState;
use raft::Ready;
use raft::SoftState;
use raft::StateRole;
use raft::Storage;
use tokio::sync::oneshot;
use tracing::debug;
use tracing::error;
use tracing::info;
use tracing::trace;
use tracing::warn;
use tracing::Level;
use uuid::Uuid;

use crate::prelude::ConfChange;
use crate::prelude::ConfChangeSingle;
use crate::prelude::ConfChangeV2;
use crate::prelude::ConfState;
use crate::prelude::MembershipChangeData;
use crate::prelude::ReplicaDesc;
use crate::prelude::Snapshot;

use super::error::Error;
use super::error::RaftGroupError;
use super::error::WriteError;
use super::event::EventChannel;
use super::event::LeaderElectionEvent;
use super::msg::ApplyData;
use super::msg::ReadIndexData;
use super::msg::WriteData;
use super::multiraft::NO_NODE;
use super::node::NodeManager;
use super::node::ResponseCallback;
use super::node::ResponseCallbackQueue;
use super::proposal::Proposal;
use super::proposal::ProposalQueue;
use super::proposal::ReadIndexProposal;
use super::proposal::ReadIndexQueue;
use super::replica_cache::ReplicaCache;
use super::response::AppWriteResponse;
use super::storage::MultiRaftStorage;
use super::transport;
use super::util;
use super::Event;

#[derive(Debug, Default, Clone, PartialEq)]
pub struct RaftGroupApplyState {
    pub commit_index: u64,
    pub commit_term: u64,
    pub applied_term: u64,
    pub applied_index: u64,
}

pub enum Status {
    None,
    Delete,
}

#[derive(Debug, Default, PartialEq)]
pub struct RaftGroupState {
    pub group_id: u64,
    pub replica_id: u64,
    // pub hard_state: HardState,
    pub soft_state: SoftState,
    pub membership_state: ConfState,
    pub apply_state: RaftGroupApplyState,
}

#[derive(Default, Debug)]
pub struct RaftGroupWriteRequest {
    pub replica_id: u64,
    pub ready: Option<Ready>,
    pub light_ready: Option<LightReady>,
}

/// Represents a replica of a raft group.
pub struct RaftGroup<RS: Storage, RES: AppWriteResponse> {
    /// Indicates the id of the node where the group resides.
    pub node_id: u64,

    pub group_id: u64,
    pub replica_id: u64,
    pub raft_group: RawNode<RS>,
    // track the nodes which members ofq the raft consensus group
    pub node_ids: Vec<u64>,
    pub proposals: ProposalQueue<RES>,
    pub leader: ReplicaDesc,
    pub committed_term: u64,
    pub state: RaftGroupState,
    pub status: Status,
    pub read_index_queue: ReadIndexQueue,
}

//===----------------------------------------------------------------------===//
// The raft group internal state
//===----------------------------------------------------------------------===//
impl<RS, RES> RaftGroup<RS, RES>
where
    RS: Storage,
    RES: AppWriteResponse,
{
    #[inline]
    pub(crate) fn is_leader(&self) -> bool {
        self.raft_group.raft.state == StateRole::Leader
    }

    #[inline]
    pub(crate) fn term(&self) -> u64 {
        self.raft_group.raft.term
    }

    #[inline]
    pub(crate) fn last_index(&self) -> u64 {
        self.raft_group.raft.raft_log.last_index()
    }
}

//===----------------------------------------------------------------------===//
// Handle raft group ready
//===----------------------------------------------------------------------===//
impl<RS, RES> RaftGroup<RS, RES>
where
    RS: Storage,
    RES: AppWriteResponse,
{
    #[tracing::instrument(
        level = Level::TRACE,
        name = "RaftGroup::handle_ready",
        skip_all,
        fields(node_id=node_id, group_id=self.group_id)
    )]
    pub(crate) async fn handle_ready<TR: transport::Transport, MRS: MultiRaftStorage<RS>>(
        &mut self,
        node_id: u64,
        transport: &TR,
        storage: &MRS,
        replica_cache: &mut ReplicaCache<RS, MRS>,
        node_manager: &mut NodeManager,
        multi_groups_write: &mut HashMap<u64, RaftGroupWriteRequest>,
        multi_groups_apply: &mut HashMap<u64, ApplyData<RES>>,
        event_bcast: &mut EventChannel,
        // pending_events: &mut Vec<Event>,
    ) {
        debug!(
            "node {}: group = {} is now ready for processing",
            node_id, self.group_id
        );
        let group_id = self.group_id;
        let mut rd = self.raft_group.ready();

        // we need to know which replica in raft group is ready.
        let replica_desc = match replica_cache.replica_for_node(group_id, node_id).await {
            Err(err) => {
                error!(
                    "node {}: write is error, got {} group replica  of storage error {}",
                    node_id, group_id, err
                );
                return;
            }
            Ok(replica_desc) => match replica_desc {
                Some(replica_desc) => {
                    assert_eq!(replica_desc.replica_id, self.raft_group.raft.id);
                    replica_desc
                }
                None => {
                    // if we can't look up the replica in storage, but the group is ready,
                    // we know that one of the replicas must be ready, so we can repair the
                    // storage to store this replica.
                    let repaired_replica_desc = ReplicaDesc {
                        node_id,
                        replica_id: self.raft_group.raft.id,
                    };
                    if let Err(err) = replica_cache
                        .cache_replica_desc(group_id, repaired_replica_desc.clone(), true)
                        .await
                    {
                        error!(
                            "write is error, got {} group replica  of storage error {}",
                            group_id, err
                        );
                        return;
                    }
                    repaired_replica_desc
                }
            },
        };

        // TODO: cache storage in related raft group.
        let gs = match storage
            .group_storage(group_id, replica_desc.replica_id)
            .await
        {
            Ok(gs) => gs,
            Err(err) => {
                error!(
                    "node({}) group({}) ready but got group storage error {}",
                    node_id, group_id, err
                );
                return;
            }
        };

        // send out messages
        if !rd.messages().is_empty() {
            transport::send_messages(
                node_id,
                transport,
                replica_cache,
                node_manager,
                group_id,
                rd.take_messages(),
            )
            .await;
        }

        if let Some(ss) = rd.ss() {
            self.handle_soft_state_change(node_id, ss, replica_cache, event_bcast)
                .await;
        }

        if !rd.read_states().is_empty() {
            self.on_reads_ready(rd.take_read_states())
        }

        // make apply task if need to apply commit entries
        if !rd.committed_entries().is_empty() {
            // insert_commit_entries will update latest commit term by commit entries.
            self.handle_committed_entries(
                node_id,
                &gs,
                replica_desc.replica_id,
                rd.take_committed_entries(),
                multi_groups_apply,
            );
        }

        // make write task if need to write disk.
        multi_groups_write.insert(
            group_id,
            RaftGroupWriteRequest {
                replica_id: replica_desc.replica_id,
                ready: Some(rd),
                light_ready: None,
            },
        );
    }

    // #[tracing::instrument(
    //     level = Level::TRACE,
    //     name = "RaftGroup:handle_committed_entries",
    //     skip_all
    // )]
    fn handle_committed_entries(
        &mut self,
        node_id: u64,
        gs: &RS,
        replica_id: u64,
        entries: Vec<Entry>,
        multi_groups_apply: &mut HashMap<u64, ApplyData<RES>>,
    ) {
        debug!(
            "node {}: create apply entries [{}, {}], group = {}, replica = {}",
            node_id,
            entries[0].index,
            entries[entries.len() - 1].index,
            self.group_id,
            replica_id
        );
        let group_id = self.group_id;
        let last_term = entries[entries.len() - 1].term;
        self.maybe_update_committed_term(last_term);

        let apply = self.create_apply(gs, replica_id, entries);
        multi_groups_apply.insert(group_id, apply);
    }

    /// Update the term of the latest entries committed during
    /// the term of the leader.
    #[inline]
    fn maybe_update_committed_term(&mut self, term: u64) {
        if self.committed_term != term && self.leader.replica_id != 0 {
            self.committed_term = term
        }
    }

    fn create_apply(&mut self, gs: &RS, replica_id: u64, entries: Vec<Entry>) -> ApplyData<RES> {
        let current_term = self.raft_group.raft.term;
        // TODO: min(persistent, committed)
        // let commit_index = self.raft_group.raft.raft_log.committed;
        let commit_index = std::cmp::min(
            self.raft_group.raft.raft_log.committed,
            self.raft_group.raft.raft_log.persisted,
        );
        let commit_term = gs.term(commit_index).unwrap();
        let mut proposals = Vec::new();
        if !self.proposals.is_empty() {
            for entry in entries.iter() {
                trace!(
                    "try find propsal with entry ({}, {}, {:?}) on replica {} in proposals {:?}",
                    entry.index,
                    entry.term,
                    entry.data,
                    replica_id,
                    self.proposals
                );
                match self
                    .proposals
                    .find_proposal(entry.term, entry.index, current_term)
                {
                    None => {
                        trace!(
                            "can't find entry ({}, {}) related proposal on replica {}",
                            entry.index,
                            entry.term,
                            replica_id
                        );
                        continue;
                    }

                    Some(p) => proposals.push(p),
                };
            }
        }

        // trace!("find proposals {:?} on replica {}", proposals, replica_id);

        let entries_size = entries
            .iter()
            .map(|ent| util::compute_entry_size(ent))
            .sum::<usize>();
        let apply = ApplyData {
            replica_id,
            group_id: self.group_id,
            term: current_term,
            commit_index,
            commit_term,
            entries,
            entries_size,
            proposals,
        };

        // trace!("make apply {:?}", apply);

        apply
    }

    fn on_reads_ready(&mut self, rss: Vec<ReadState>) {
        self.read_index_queue.advance_reads(rss);
        while let Some(p) = self.read_index_queue.pop_front() {
            p.tx.map(|tx| tx.send(Ok(())));
        }
    }

    // Dispatch soft state changed related events.
    async fn handle_soft_state_change<MRS: MultiRaftStorage<RS>>(
        &mut self,
        node_id: u64,
        ss: &SoftState,
        replica_cache: &mut ReplicaCache<RS, MRS>,
        // pending_events: &mut Vec<Event>,
        event_bcast: &mut EventChannel,
    ) {
        if ss.leader_id != 0 && ss.leader_id != self.leader.replica_id {
            return self
                .handle_leader_change(node_id, ss, replica_cache, event_bcast)
                .await;
        }
    }

    // Process soft state changed on leader changed
    #[tracing::instrument(
        level = Level::TRACE,
        name = "RaftGroup::handle_leader_change", 
        skip_all
    )]
    async fn handle_leader_change<MRS: MultiRaftStorage<RS>>(
        &mut self,
        node_id: u64,
        ss: &SoftState,
        replica_cache: &mut ReplicaCache<RS, MRS>,
        // pending_events: &mut Vec<Event>,
        event_bcast: &mut EventChannel,
    ) {
        let group_id = self.group_id;
        let replica_desc = match replica_cache
            .replica_desc(self.group_id, ss.leader_id)
            .await
        {
            Err(err) => {
                error!("group({}) replica({}) become leader, but got it replica description for node id error {}",
            group_id, ss.leader_id, err);
                // FIXME: it maybe temporary error, retry or use NO_NODE.
                return;
            }
            Ok(op) => op,
        };

        let replica_desc = match replica_desc {
            Some(desc) => desc,
            None => {
                // this means that we do not know which node the leader is on,
                // but this does not affect us to send LeaderElectionEvent, as
                // this will be fixed by subsequent message communication.
                // TODO: and asynchronous broadcasting
                warn!(
                    "replica {} of raft group {} becomes leader, but  node id is not known",
                    ss.leader_id, group_id
                );

                ReplicaDesc {
                    node_id: NO_NODE,
                    replica_id: ss.leader_id,
                }
            }
        };

        info!(
            "node {}: group = {}, replica = {} became leader",
            node_id, self.group_id, ss.leader_id
        );
        let replica_id = replica_desc.replica_id;
        self.leader = replica_desc; // always set because node_id maybe NO_NODE.
        event_bcast.push(Event::LederElection(LeaderElectionEvent {
            group_id: self.group_id,
            leader_id: ss.leader_id,
            replica_id,
        }));
    }

    #[tracing::instrument(
        level = Level::TRACE,
        name = "RaftGroup::handle_write",
        skip_all,
        fields(node_id=node_id, group_id=self.group_id)
    )]
    pub(crate) async fn handle_ready_write<TR: transport::Transport, MRS: MultiRaftStorage<RS>>(
        &mut self,
        node_id: u64,
        gwr: &mut RaftGroupWriteRequest,
        gs: &RS,
        transport: &TR,
        replica_cache: &mut ReplicaCache<RS, MRS>,
        node_manager: &mut NodeManager,
    ) {
        let group_id = self.group_id;
        // TODO: cache storage in RaftGroup

        let mut ready = gwr.ready.take().unwrap();
        if *ready.snapshot() != Snapshot::default() {
            let snapshot = ready.snapshot().clone();
            // FIXME: handle error
            gs.apply_snapshot(snapshot).unwrap();
        }

        if !ready.entries().is_empty() {
            let entries = ready.take_entries();
            debug!(
                "node {}: append entries [{}, {}]",
                node_id,
                entries[0].index,
                entries[entries.len() - 1].index
            );
            if let Err(_error) = gs.append_entries(&entries) {
                // FIXME: handle error
                panic!("node {}: append entries error = {}", node_id, _error);
            }
        }

        if let Some(hs) = ready.hs() {
            let hs = hs.clone();
            if let Err(_error) = gs.set_hardstate(hs) {
                // FIXME: handle error
                panic!("node {}: set hardstate error = {}", node_id, _error);
            }
        }

        if !ready.persisted_messages().is_empty() {
            transport::send_messages(
                node_id,
                transport,
                replica_cache,
                node_manager,
                group_id,
                ready.take_persisted_messages(),
            )
            .await;
        }

        let light_ready = self.raft_group.advance_append(ready);
        // self.raft_group
        //    .advance_apply_to(self.state.apply_state.applied_index);
        gwr.light_ready = Some(light_ready);
    }

    #[tracing::instrument(
        level = Level::TRACE,
        name = "RaftGroup::handle_light_ready",
        skip_all,
        fields(node_id=node_id, group_id=self.group_id)
    )]
    pub async fn handle_light_ready<TR: transport::Transport, MRS: MultiRaftStorage<RS>>(
        &mut self,
        node_id: u64,
        transport: &TR,
        storage: &MRS,
        replica_cache: &mut ReplicaCache<RS, MRS>,
        node_manager: &mut NodeManager,
        gwr: &mut RaftGroupWriteRequest,
        multi_groups_apply: &mut HashMap<u64, ApplyData<RES>>,
    ) {
        let group_id = self.group_id;
        let replica_id = gwr.replica_id;
        let mut light_ready = gwr.light_ready.take().unwrap();
        // let group_storage = self
        //     .storage
        //     .group_storage(group_id, gwr.replica_id)
        //     .await
        //     .unwrap();

        if let Some(commit) = light_ready.commit_index() {
            debug!("node {}: set commit = {}", node_id, commit);
            // group_storage.set_commit(commit);
        }

        if !light_ready.messages().is_empty() {
            let messages = light_ready.take_messages();
            transport::send_messages(
                node_id,
                transport,
                replica_cache,
                node_manager,
                group_id,
                messages,
            )
            .await;
        }

        if !light_ready.committed_entries().is_empty() {
            debug!("node {}: light ready has committed entries", node_id);
            // TODO: cache storage in related raft group.
            let gs = match storage.group_storage(group_id, gwr.replica_id).await {
                Ok(gs) => gs,
                Err(err) => {
                    error!(
                        "node({}) group({}) ready but got group storage error {}",
                        node_id, group_id, err
                    );
                    return;
                }
            };
            self.handle_committed_entries(
                node_id,
                &gs,
                replica_id,
                light_ready.take_committed_entries(),
                multi_groups_apply,
            );
        }
        // FIXME: always advance apply
        // TODO: move to upper layer
        // self.raft_group.advance_apply();
    }

    fn pre_propose_write(&mut self, write_data: &WriteData<RES>) -> Result<(), Error>
    where
        RS: Storage,
    {
        if write_data.data.is_empty() {
            return Err(Error::BadParameter(
                "write request data must not be empty".to_owned(),
            ));
        }

        if !self.is_leader() {
            return Err(Error::Write(WriteError::NotLeader {
                node_id: self.node_id,
                group_id: self.group_id,
                replica_id: self.replica_id,
            }));
        }

        if write_data.term != 0 && self.term() > write_data.term {
            return Err(Error::Write(WriteError::Stale(
                write_data.term,
                self.term(),
            )));
        }

        Ok(())
    }

    pub fn propose_write(
        &mut self,
        write_data: WriteData<RES>,
        // tx: oneshot::Sender<Result<RES, Error>>,
    ) -> Option<ResponseCallback> {
        if let Err(err) = self.pre_propose_write(&write_data) {
            return Some(ResponseCallbackQueue::new_error_callback(
                write_data.tx,
                err,
            ));
        }
        let term = self.term();

        // propose to raft group
        let next_index = self.last_index() + 1;
        if let Err(err) = self.raft_group.propose(
            write_data.context.map_or(vec![], |ctx_data| ctx_data),
            write_data.data,
        ) {
            return Some(ResponseCallbackQueue::new_error_callback(
                write_data.tx,
                Error::Raft(err),
            ));
        }

        let index = self.last_index() + 1;
        if next_index == index {
            return Some(ResponseCallbackQueue::new_error_callback(
                write_data.tx,
                Error::Write(WriteError::UnexpectedIndex(next_index, index - 1)),
            ));
        }

        let proposal = Proposal {
            index: next_index,
            term,
            is_conf_change: false,
            tx: Some(write_data.tx),
        };

        self.proposals.push(proposal);
        None
    }

    pub fn read_index_propose(
        &mut self,
        data: ReadIndexData,
    ) -> Option<ResponseCallback> {
        // let uuid = Uuid::new_v4();
        let mut ctx = Vec::new();
        // Safety: This method is unsafe because it is unsafe to
        // transmute typed allocations to binary. Furthermore, Rust currently
        // indicates that it is undefined behavior to observe padding bytes,
        // which will happen when we memmcpy structs which contain padding bytes.
        unsafe { abomonation::encode(&data.context, &mut ctx).unwrap() };
        
        self.raft_group.read_index(ctx);

        let proposal = ReadIndexProposal {
            uuid: data.context.uuid,
            read_index: None,
            context: None,
            tx: Some(data.tx),
        };
        self.read_index_queue.push_back(proposal);
        None
    }

    fn pre_propose_membership(&mut self, data: &MembershipChangeData) -> Result<(), Error>
    where
        RS: Storage,
    {
        if data.group_id == 0 {
            return Err(Error::BadParameter(
                "group id must be more than 0".to_owned(),
            ));
        }

        if data.changes.is_empty() {
            return Err(Error::BadParameter("group id must not be empty".to_owned()));
        }

        if !self.is_leader() {
            return Err(Error::Write(WriteError::NotLeader {
                node_id: self.node_id,
                group_id: self.group_id,
                replica_id: self.replica_id,
            }));
        }

        if data.term != 0 && self.term() > data.term {
            return Err(Error::Write(WriteError::Stale(data.term, self.term())));
        }

        Ok(())
    }

    pub fn propose_membership_change(
        &mut self,
        data: MembershipChangeData,
        tx: oneshot::Sender<Result<RES, Error>>,
    ) -> Option<ResponseCallback> {
        // TODO: add pre propose check

        if let Err(err) = self.pre_propose_membership(&data) {
            return Some(ResponseCallbackQueue::new_error_callback(tx, err));
        }

        let term = self.term();

        info!(
            "node {}: propose membership change: request = {:?}",
            0, /* TODO: add it*/ data
        );

        let next_index = self.last_index() + 1;

        let res = if data.changes.len() == 1 {
            let (ctx, cc) = to_cc(&data);
            self.raft_group.propose_conf_change(ctx, cc)
        } else {
            let (ctx, cc) = to_ccv2(&data);
            self.raft_group.propose_conf_change(ctx, cc)
        };

        if let Err(err) = res {
            error!(
                "node {}: propose membership change error: error = {}",
                0, /* TODO: add it*/ err
            );
            return Some(ResponseCallbackQueue::new_error_callback(
                tx,
                Error::Raft(err),
            ));
        }

        let index = self.last_index() + 1;
        if next_index == index {
            error!(
                "node {}: propose membership failed, expect log index = {}, got = {}",
                0, /* TODO: add it*/
                next_index,
                index - 1,
            );

            return Some(ResponseCallbackQueue::new_error_callback(
                tx,
                Error::Write(WriteError::UnexpectedIndex(next_index, index - 1)),
            ));
        }

        let proposal = Proposal {
            index: next_index,
            term,
            is_conf_change: true,
            tx: Some(tx),
        };

        self.proposals.push(proposal);
        None
    }

    /// Remove pending proposals.
    pub(crate) fn remove_pending_proposals(&mut self) {
        let proposals = self.proposals.drain(..);
        for proposal in proposals.into_iter() {
            let err = Err(Error::RaftGroup(RaftGroupError::Deleted(
                self.group_id,
                self.replica_id,
            )));
            // TODO: move to event queue
            proposal.tx.map(|tx| tx.send(err));
        }
    }

    pub(crate) fn add_track_node(&mut self, node_id: u64) {
        if self.node_ids.iter().position(|id| *id == node_id).is_none() {
            self.node_ids.push(node_id)
        }
    }

    /// Remove the node in the group where the replica is located in the tracing nodes.
    /// return `false` if the nodes traced cannot find the given `node_id`, `true` otherwise.
    pub(crate) fn remove_track_node(&mut self, node_id: u64) -> bool {
        let len = self.node_ids.len();
        self.node_ids
            .iter()
            .position(|id| node_id == *id)
            .map_or(false, |idx| {
                self.node_ids.swap(idx, len - 1);
                self.node_ids.truncate(len - 1);
                true
            })
    }
}

fn to_cc(data: &MembershipChangeData) -> (Vec<u8>, ConfChange) {
    assert_eq!(data.changes.len(), 1);
    let mut cc = ConfChange::default();
    cc.set_change_type(data.changes[0].change_type());
    // TODO: set membership change id
    cc.node_id = data.changes[0].replica_id;
    (data.encode_to_vec(), cc)
}

fn to_ccv2(data: &MembershipChangeData) -> (Vec<u8>, ConfChangeV2) {
    assert!(data.changes.len() > 1);
    let mut cc = ConfChangeV2::default();
    let mut sc = vec![];
    for change in data.changes.iter() {
        sc.push(ConfChangeSingle {
            change_type: change.change_type,
            node_id: change.replica_id,
        });
    }

    // TODO: consider setting transaction type
    cc.set_changes(sc);
    (data.encode_to_vec(), cc)
}