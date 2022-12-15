use std::collections::VecDeque;
use std::collections::hash_map::HashMap;
use std::hash::Hash;
use std::marker::PhantomData;
use std::time::Duration;

use prost::Message as ProstMessage;
use raft::RawNode;
use raft::Ready;
use smallvec::SmallVec;
use tokio::sync::mpsc::channel;
use tokio::sync::mpsc::Receiver;
use tokio::sync::mpsc::Sender;
use tokio::sync::oneshot;
use tokio::sync::watch;
use tokio::task::JoinHandle;
use tokio::time::interval;

use tracing::error;
use tracing::info;
use tracing::trace;

use raft::Config;

use super::config::GroupConfig;
use super::error::Error;
use super::error::ProposalError;
use super::error::RaftGroupError;
use super::proposal::Proposal;
use super::proposal::ReadIndexProposal;
use super::transport::MessageInterface;
use super::transport::Transport;
use super::write::GroupWriteRequest;
use super::write::WriteAddress;
use super::write::WriteTaskRequest;
use super::write::WriteTaskResponse;
use super::write::WriterActor;

use crate::proto::AppReadIndexRequest;
use crate::proto::AppReadIndexResponse;
use crate::proto::AppWriteRequest;
use crate::proto::AppWriteResponse;
use crate::proto::Message;
use crate::proto::MessageType;
use crate::proto::RaftGroupManagementMessage;
use crate::proto::RaftGroupManagementMessageType;
use crate::proto::RaftMessage;
use crate::proto::ReadIndexContext;
use crate::proto::ReplicaMetadata;
use crate::storage::transmute_message;
use crate::storage::transmute_raft_snapshot_metadata;
use crate::storage::MultiRaftStorage;
use crate::storage::RaftStorage;
use crate::storage::RaftStorageImpl;
use crate::storage::StorageError;
// use crate::proto::Error;

const NO_GORUP: u64 = 0;
const NO_NODE: u64 = 0;

/// node represents a physical node and contains a group of rafts.
pub struct Node {
    node_id: u64,
    group_map: HashMap<u64, ()>,
}

pub struct RaftGroup<RS: RaftStorage> {
    group_id: u64,
    replica_id: u64,
    raft_group: RawNode<RaftStorageImpl<RS>>,
    // track the nodes which members ofq the raft consensus group
    node_ids: Vec<u64>,
    leader: Option<ReplicaMetadata>,
}

/// MultiRaft represents a group of raft replicas
pub struct MultiRaft<MI, T, RS, MRS>
where
    MI: MessageInterface,
    T: Transport<MI>,
    RS: RaftStorage,
    MRS: MultiRaftStorage<RS>,
{
    store_id: u64,
    config: GroupConfig,
    actor_address: MultiRaftActorAddress,
    actor_join_handle: JoinHandle<()>,
    _m1: PhantomData<MI>,
    _m2: PhantomData<T>,
    _m3: PhantomData<RS>,
    _m4: PhantomData<MRS>,
}

impl<MI, T, RS, MRS> MultiRaft<MI, T, RS, MRS>
where
    MI: MessageInterface,
    T: Transport<MI>,
    RS: RaftStorage,
    MRS: MultiRaftStorage<RS>,
{
    pub fn new(
        config: GroupConfig,
        node_id: u64,
        store_id: u64,
        transport: T,
        storage: MRS,
    ) -> Self {
        let (stop_tx, stop_rx) = watch::channel(false);
        let (actor_join_handle, actor_address) =
            MultiRaftActor::spawn(&config, node_id, store_id, transport, storage, stop_rx);
        Self {
            store_id,
            config,
            actor_address,
            actor_join_handle,
            _m1: PhantomData,
            _m2: PhantomData,
            _m3: PhantomData,
            _m4: PhantomData,
        }
    }

    pub async fn write(&self, request: AppWriteRequest) -> Result<AppWriteResponse, Error> {
        let (tx, rx) = oneshot::channel();
        if let Err(_) = self
            .actor_address
            .write_propose_tx
            .send((request, tx))
            .await
        {}

        rx.await.unwrap()
    }

    pub async fn initial_raft_group(&self, msg: RaftGroupManagementMessage) -> Result<(), Error> {
        assert_eq!(
            msg.msg_type(),
            RaftGroupManagementMessageType::MsgInitialGroup
        );
        let (tx, rx) = oneshot::channel();
        if let Err(_error) = self.actor_address.manager_group_tx.send((msg, tx)).await {
            panic!("manager group receiver dropped")
        }

        match rx.await {
            Err(_error) => panic!("sender dopped"),
            Ok(res) => res,
        }
    }

    /// Bootstrap a new raft consensus group.
    pub async fn bootstrap_raft_group(&self, group_id: u64, replica_id: u64) -> Result<(), Error> {
        let (tx, rx) = oneshot::channel();
        let mut msg = RaftGroupManagementMessage::default();
        msg.group_id = group_id;
        msg.replica_id = replica_id;
        msg.set_msg_type(RaftGroupManagementMessageType::MsgBootstrapGroup);

        if let Err(_error) = self.actor_address.manager_group_tx.send((msg, tx)).await {
            panic!("manager group receiver dropped")
        }

        match rx.await {
            Err(_error) => panic!("sender dopped"),
            Ok(res) => res,
        }
    }
}

/// MultiRaftAddress is used to communicate with MultiRaftActor
pub struct MultiRaftActorAddress {
    write_propose_tx: Sender<(
        AppWriteRequest,
        oneshot::Sender<Result<AppWriteResponse, Error>>,
    )>,
    raft_message_tx: Sender<RaftMessage>,
    manager_group_tx: Sender<(
        RaftGroupManagementMessage,
        oneshot::Sender<Result<(), Error>>,
    )>,
}

pub struct MultiRaftActor<MI, T, RS, MRS>
where
    MI: MessageInterface,
    T: Transport<MI>,
    RS: RaftStorage,
    MRS: MultiRaftStorage<RS>,
{
    store_id: u64,
    node_id: u64,
    nodes: HashMap<u64, Node>,
    groups: HashMap<u64, RaftGroup<RS>>,
    tick_interval: Duration,
    election_tick: usize,
    heartbeat_tick: usize,
    write_propose_rx: Receiver<(
        AppWriteRequest,
        oneshot::Sender<Result<AppWriteResponse, Error>>,
    )>,
    raft_message_rx: Receiver<RaftMessage>,
    manager_group_rx: Receiver<(
        RaftGroupManagementMessage,
        oneshot::Sender<Result<(), Error>>,
    )>,

    write_actor_address: WriteAddress,
    storage: MRS,
    transport: T,
    waiting_ready_groups: VecDeque<HashMap<u64, Ready>>,
    _m1: PhantomData<RS>,
    _m2: PhantomData<MI>,
}

impl<MI, T, RS, MRS> MultiRaftActor<MI, T, RS, MRS>
where
    MI: MessageInterface,
    T: Transport<MI>,
    RS: RaftStorage,
    MRS: MultiRaftStorage<RS>,
{
    // #[tracing::instrument(name = "MultiRaftActor::spawn",  skip(storage))]
    pub fn spawn(
        cfg: &GroupConfig,
        node_id: u64,
        store_id: u64,
        transport: T,
        storage: MRS,
        stop: watch::Receiver<bool>,
    ) -> (JoinHandle<()>, MultiRaftActorAddress) {
        let (raft_message_tx, raft_message_rx) = channel(1);
        let (manager_group_tx, manager_group_rx) = channel(1);

        let (write_actor_join, write_actor_address) = WriterActor::spawn(storage.clone(), stop.clone());

        // create write propose channel
        let (write_propose_tx, write_propose_rx) = channel(1);

        let actor = MultiRaftActor {
            store_id,
            node_id,
            nodes: HashMap::new(),
            groups: HashMap::new(),
            tick_interval: Duration::from_millis(cfg.tick_interval),
            election_tick: cfg.election_tick,
            heartbeat_tick: cfg.heartbeat_tick,
            write_propose_rx,
            raft_message_rx,
            manager_group_rx,
            storage,
            transport,
            write_actor_address,
            waiting_ready_groups: VecDeque::default(),
            _m1: PhantomData,
            _m2: PhantomData,
        };

        let main_loop = async move {
            actor.start(stop).await;
        };

        let join = tokio::spawn(main_loop);

        let address = MultiRaftActorAddress {
            raft_message_tx,
            manager_group_tx,
            write_propose_tx,
        };

        (join, address)
    }

    /// start actor.
    // #[tracing::instrument(name = "MultiRaftActor::start", skip(self))]
    async fn start(mut self, mut stop: watch::Receiver<bool>) {
        // Each time ticker expires, the ticks increments,
        // when ticks >= heartbeat_tick triggers the merged heartbeat.
        let mut ticks = 0;
        let mut ticker = interval(self.tick_interval);
        let mut activity_groups = vec![];
        loop {
            ticker.tick().await;
            tokio::select! {
                // handle stop
                _ = stop.changed() => {
                    if *stop.borrow() {
                        break
                    }
                }
                _ = ticker.tick() => {
                    ticks += 1;
                    if ticks >= self.heartbeat_tick {
                        ticks = 0;
                        self.coalesced_heratbeat().await;
                    }
                },
                Some(msg) = self.raft_message_rx.recv() => {
                    self.handle_raft_message(msg).await
                },
                Some((msg, tx)) = self.manager_group_rx.recv() => {
                    let group_id = msg.group_id;
                    let changed_groups = self.handle_manager_group_message(msg, tx).await;
                    activity_groups.extend(changed_groups.into_iter());
                },
                Some(write_task_response) = self.write_actor_address.rx.recv() => {
                    self.handle_write_task_response(write_task_response).await;
                },
                else => {
                    let (write_task_request, ready_groups) = self.handle_ready(&activity_groups).await;
                    activity_groups.clear();
                    if !ready_groups.is_empty() {
                        self.waiting_ready_groups.push_back(ready_groups);
                        self.write_actor_address.tx.send(write_task_request).await;
                        continue
                    }
                },
            }
        }
    }

    async fn coalesced_heratbeat(&self) {
        for (node_id, node) in self.nodes.iter() {
            if *node_id == self.node_id {
                continue;
            }

            // coalesced heartbeat to all nodes
            let mut raft_msg = Message::default();
            raft_msg.set_msg_type(MessageType::MsgHeartbeat);
            let msg = RaftMessage {
                group_id: NO_GORUP,
                from_replica: Some(ReplicaMetadata {
                    node_id: self.node_id,
                    store_id: 0,
                    replica_id: 0,
                }),
                to_replica: Some(ReplicaMetadata {
                    node_id: *node_id,
                    store_id: 0,
                    replica_id: 0,
                }),
                msg: Some(raft_msg),
            };

            if let Err(_error) = self.transport.send(msg) {}
        }
    }

    async fn handle_raft_message(&mut self, msg: RaftMessage) {
        match msg.msg.as_ref().unwrap().msg_type() {
            MessageType::MsgHeartbeat => self.fanout_heartbeat(msg).await,
            MessageType::MsgHeartbeatResponse => self.fanout_heartbeat_response(msg).await,
            _ => {}
        }
    }

    async fn fanout_heartbeat(&mut self, raft_msg: RaftMessage) {
        let from = match raft_msg.from_replica.as_ref() {
            None => return,
            Some(from) => from,
        };

        let to = match raft_msg.to_replica.as_ref() {
            None => return,
            Some(to) => to,
        };

        let send_response = || {
            let response_msg = {
                let mut msg = Message::default();
                msg.from = self.node_id;
                msg.to = from.node_id;
                msg.set_msg_type(MessageType::MsgHeartbeatResponse);
                RaftMessage {
                    group_id: NO_GORUP,
                    from_replica: Some(ReplicaMetadata {
                        node_id: self.node_id,
                        store_id: 0,
                        replica_id: 0,
                    }),
                    to_replica: Some(ReplicaMetadata {
                        node_id: from.node_id,
                        store_id: 0,
                        replica_id: 0,
                    }),
                    msg: Some(msg),
                }
            };

            self.transport.send(response_msg).unwrap();
        };

        let node = match self.nodes.get(&from.node_id) {
            None => {
                send_response();
                return;
            }
            Some(node) => node,
        };

        let mut fanouted_groups = 0;
        let mut fanouted_followers = 0;
        for (group_id, _) in node.group_map.iter() {
            let group = match self.groups.get_mut(group_id) {
                None => continue,
                Some(group) => group,
            };

            fanouted_groups += 1;

            let leader = match group.leader.as_ref() {
                None => continue,
                Some(leader) => leader,
            };

            if leader.node_id != from.node_id || from.node_id != self.node_id {
                continue;
            }

            // gets the replica stored in this node.
            let from_replica_id = self
                .storage
                .replica_in_node(*group_id, from.node_id)
                .await
                .unwrap()
                .unwrap();
            let to_replica_id = self
                .storage
                .replica_in_node(*group_id, to.node_id)
                .await
                .unwrap()
                .unwrap();
            fanouted_followers += 1;
            let mut msg = raft::prelude::Message::default();
            msg.set_msg_type(raft::prelude::MessageType::MsgHeartbeat);
            msg.from = from_replica_id;
            msg.to = to_replica_id;
            group.raft_group.step(msg).unwrap();
        }

        send_response();
    }

    async fn fanout_heartbeat_response(&mut self, raft_msg: RaftMessage) {
        let from = match raft_msg.from_replica.as_ref() {
            None => return,
            Some(from) => from,
        };

        let to = match raft_msg.to_replica.as_ref() {
            None => return,
            Some(to) => to,
        };

        let node = match self.nodes.get(&from.node_id) {
            None => return,
            Some(node) => node,
        };

        for (group_id, _) in node.group_map.iter() {
            let group = match self.groups.get_mut(group_id) {
                None => continue,
                Some(group) => group,
            };

            let leader = match group.leader.as_ref() {
                None => continue,
                Some(leader) => leader,
            };

            if leader.node_id != from.node_id || from.node_id != self.node_id {
                continue;
            }

            // gets the replica stored in this node.
            let from_replica_id = self
                .storage
                .replica_in_node(*group_id, from.node_id)
                .await
                .unwrap()
                .unwrap();
            let to_replica_id = self
                .storage
                .replica_in_node(*group_id, to.node_id)
                .await
                .unwrap()
                .unwrap();

            let mut msg = raft::prelude::Message::default();
            msg.set_msg_type(raft::prelude::MessageType::MsgHeartbeatResponse);
            msg.from = from_replica_id;
            msg.to = to_replica_id;
            group.raft_group.step(msg).unwrap();
        }
    }

    async fn step_raft_message(&mut self, mut raft_msg: RaftMessage) {
        assert_ne!(raft_msg.group_id, NO_GORUP);

        let to_replica = raft_msg.to_replica.take().unwrap();

        let group = match self.groups.get_mut(&raft_msg.group_id) {
            Some(group) => group,
            None => {
                let group_id = raft_msg.group_id;
                let replica_id = to_replica.replica_id;
                self.bootstrap_group_replica(group_id, replica_id)
                    .await
                    .unwrap();
                self.groups.get_mut(&group_id).unwrap()
            }
        };

        let msg = raft_msg.msg.take().unwrap();
        group.raft_group.step(transmute_message(msg)).unwrap();
    }

    #[tracing::instrument(name = "MultiRaftActor::handle_manager_group_message", skip(self))]
    async fn handle_manager_group_message(
        &mut self,
        msg: RaftGroupManagementMessage,
        tx: oneshot::Sender<Result<(), Error>>,
    ) -> Vec<u64> {
        let mut activity_groups = vec![];
        let res = match msg.msg_type() {
            RaftGroupManagementMessageType::MsgInitialGroup => {
                activity_groups.push(msg.group_id);
                self.initial_group(msg).await
            }
            RaftGroupManagementMessageType::MsgBootstrapGroup => {
                activity_groups.push(msg.group_id);
                self.bootstrap_group_replica(msg.group_id, msg.replica_id)
                    .await
            }
            RaftGroupManagementMessageType::MsgRemoveGoup => todo!(),
        };

        if let Err(_error) = tx.send(res) {}
        activity_groups
    }

    /// Initial the raft consensus group and start a replica in current node.
    async fn initial_group(&mut self, msg: RaftGroupManagementMessage) -> Result<(), Error> {
        assert_eq!(
            msg.msg_type(),
            RaftGroupManagementMessageType::MsgInitialGroup
        );

        if msg.group_id == 0 {
            return Err(Error::BadParameter(format!("bad group_id parameter (0)")));
        }

        if msg.replica_id == 0 {
            return Err(Error::BadParameter(format!("bad replica_id parameter (0)")));
        }

        if self.groups.contains_key(&msg.group_id) {
            return Err(Error::RaftGroupAlreayExists(msg.group_id));
        }

        // get the raft consensus group reated to storage, create if not exists.
        let group_storage = match self
            .storage
            .group_storage(msg.group_id, msg.replica_id)
            .await
            .map_err(|err| Error::Store(err))?
        {
            Some(gs) => gs,
            None => {
                let conf_state = (msg.initial_voters.clone(), vec![]); // TODO: learner
                self.storage
                    .create_group_storage_with_conf_state(msg.group_id, msg.group_id, conf_state)
                    .await
                    .map_err(|err| Error::Store(err))?
            }
        };

        // create raft consensus group with default logger and group storage.
        let applied = 0;
        let raft_cfg = raft::Config {
            id: msg.replica_id,
            applied,
            election_tick: self.election_tick,
            heartbeat_tick: self.heartbeat_tick,
            max_size_per_msg: 1024 * 1024,
            max_inflight_msgs: 256,
            ..Default::default()
        };

        let raft_store = group_storage.clone();
        let raft_group = raft::RawNode::with_default_logger(&raft_cfg, raft_store)
            .map_err(|err| Error::RaftGroup(err))?;

        // add group to node map
        self.add_node(self.node_id, msg.group_id);

        // insert raft_group to group map
        let group = RaftGroup {
            group_id: msg.group_id,
            replica_id: msg.replica_id,
            raft_group,
            node_ids: vec![self.node_id],
            leader: None,
        };
        self.groups.insert(msg.group_id, group);

        Ok(())
    }

    /// Bootstrap a replica of the raft consensus group on this node.
    #[tracing::instrument(name = "MultiRaftActor::bootstrap_group", skip(self))]
    async fn bootstrap_group_replica(
        &mut self,
        group_id: u64,
        replica_id: u64,
    ) -> Result<(), Error> {
        if group_id == 0 {
            return Err(Error::BadParameter(format!("bad group_id parameter (0)")));
        }

        if replica_id == 0 {
            return Err(Error::BadParameter(format!("bad replica_id parameter (0)")));
        }

        if self.groups.contains_key(&group_id) {
            return Err(Error::RaftGroupAlreayExists(group_id));
        }

        // let mut replica_id = 0;
        // get the raft consensus group reated to storage, create if not exists.
        let group_storage = match self
            .storage
            .group_storage(group_id, replica_id)
            .await
            .map_err(|err| Error::Store(err))?
        {
            Some(gs) => gs,
            None => return Err(Error::Store(StorageError::Unavailable)),
        };

        let rs = group_storage
            .initial_state()
            .map_err(|err| Error::Store(err))?;

        let voters = rs.conf_state.voters;
        // for voter_id in voters.iter() {
        //     let replica_metadata = self
        //         .storage
        //         .replica_metadata(group_id, *voter_id)
        //         .await.map_err(|err| Error::Store(err))?;

        //     if self.store_id == replica_metadata.store_id {
        //         if replica_id == 0 {
        //             replica_id = *voter_id;
        //         } else if replica_id < *voter_id {

        //         }
        //         break;
        //     }
        // }

        if replica_id == 0 {
            return Err(Error::RaftGroupError(RaftGroupError::BootstrapError(
                group_id,
                self.store_id,
            )));
        }

        let applied = 0;
        let raft_cfg = raft::Config {
            id: replica_id,
            applied,
            election_tick: self.election_tick,
            heartbeat_tick: self.heartbeat_tick,
            max_size_per_msg: 1024 * 1024,
            max_inflight_msgs: 256,
            ..Default::default()
        };

        let raft_store = group_storage.clone();
        let raft_group = raft::RawNode::with_default_logger(&raft_cfg, raft_store)
            .map_err(|err| Error::RaftGroup(err))?;

        let mut group = RaftGroup {
            group_id,
            replica_id,
            raft_group,
            node_ids: Vec::new(),
            leader: None,
        };

        for voter_id in voters.iter() {
            let replica_metadata = self
                .storage
                .replica_metadata(group_id, *voter_id)
                .await
                .map_err(|err| Error::Store(err))?;

            // at this point, we don't know the infomation about
            // the node which replica.
            if replica_metadata.node_id == NO_NODE {
                continue;
            }

            // track the nodes which other members of the raft consensus group
            group.node_ids.push(replica_metadata.node_id);
            self.add_node(replica_metadata.node_id, group_id);
        }

        self.groups.insert(group_id, group);

        Ok(())
    }

    fn add_node(&mut self, node_id: u64, group_id: u64) {
        let node = match self.nodes.get_mut(&node_id) {
            None => {
                self.nodes.insert(
                    node_id,
                    Node {
                        node_id,
                        group_map: HashMap::new(),
                    },
                );
                self.nodes.get_mut(&node_id).unwrap()
            }
            Some(node) => node,
        };

        assert_ne!(group_id, 0);
        node.group_map.insert(group_id, ());
    }

    async fn handle_write_request(
        &mut self,
        request: AppWriteRequest,
    ) -> Result<Proposal<AppWriteResponse>, Error> {
        let group_id = request.group_id;
        let group = self.groups.get_mut(&group_id).unwrap();
        let term = group.raft_group.raft.term;
        if request.term != 0 && term > request.term {
            return Err(Error::Proposal(ProposalError::Stale(term)));
        }

        // propose to raft gorup
        let expected_next_index = group.raft_group.raft.raft_log.last_index() + 1;
        let _ = group
            .raft_group
            .propose(request.context, request.data)
            .map_err(|err| Error::Proposal(ProposalError::Other(Box::new(err))))?;

        let index = group.raft_group.raft.raft_log.last_index() + 1;
        if expected_next_index != index {
            return Err(Error::Proposal(ProposalError::Unexpected(index)));
        }

        Ok(Proposal::<AppWriteResponse> {
            index,
            term,
            is_conf_change: false,
            tx: None,
        })
    }

    async fn handle_read_index_request(
        &mut self,
        request: AppReadIndexRequest,
    ) -> Result<ReadIndexProposal<AppReadIndexResponse>, Error> {
        let group_id = request.group_id;
        let uuid = uuid::Uuid::new_v4();

        let group = self.groups.get_mut(&group_id).unwrap();
        let term = group.raft_group.raft.term;
        let read_context = match request.context {
            None => vec![],
            Some(ctx) => ctx.encode_length_delimited_to_vec(),
        };

        group.raft_group.read_index(read_context);

        Ok(ReadIndexProposal::<AppReadIndexResponse> {
            uuid,
            read_index: None,
            context: None,
            tx: None,
        })
    }

    async fn handle_ready(&mut self, activity_groups: &Vec<u64>) -> (WriteTaskRequest, HashMap<u64, Ready>) {
        let mut ready_groups = HashMap::new();
        let mut write_task_request = WriteTaskRequest::default();
        for group_id in activity_groups.iter() {
            let group = match self.groups.get_mut(group_id) {
                None => continue,
                Some(group) => group,
            };

            if !group.raft_group.has_ready() {
                continue;
            }

            let mut ready_group = group.raft_group.ready();
            // we need to know which replica in raft group is ready.

            let replica = match self.storage.replica_in_node(*group_id, self.store_id).await {
                Err(error) => {
                    error!("storage error: {}", error);
                    continue;
                }
                Ok(replica) => replica,
            };

            // if we can't look up the replica in storage, but the group is ready,
            // we know that one of the replicas must be ready, so we can repair the
            // storage to store this replica.
            if replica.is_none() {
                // group.raft_group.raft.id
            }

            // make write task if need to write disk.
            // TODO: batch (yuanchang xu)
            let mut write_task = GroupWriteRequest::default();
            let mut has_write = false;
            if !ready_group.snapshot().is_empty() {
                write_task.snapshot = Some(ready_group.snapshot().clone());
                has_write = true;
            }
            if let Some(hs) = ready_group.hs() {
                write_task.hard_state = Some(hs.clone());
                has_write = true;
            }
            if !ready_group.entries().is_empty() {
                write_task.entries = Some(ready_group.take_entries());
                has_write = true;
            }
            if has_write {
                write_task_request.groups.insert(*group_id, write_task);
            }

            // make apply task if need to apply commit entries
            if !ready_group.committed_entries().is_empty() {}

            ready_groups.insert(*group_id, ready_group);
        }

        // if !write_task_request.groups.is_empty() {
        //     self.write_actor_address.tx.send(write_task_request).await;
        // }

        return (write_task_request, ready_groups);
    }

    async fn handle_write_task_response(&mut self, response: WriteTaskResponse)  {
        if self.waiting_ready_groups.is_empty() {
            return
        }

        let ready_groups = match self.waiting_ready_groups.pop_front() {
            Some(ready_groups) => ready_groups,
            None => return,
        };

        for (group_id, ready_group) in ready_groups.into_iter() {
            let mut_group = match self.groups.get_mut(&group_id) {
                None => continue,
                Some(g) => g,
            };

            let replica_id = self.storage.replica_in_node(group_id, self.node_id).await.unwrap().unwrap();
            let group_storage = self.storage.group_storage(group_id, replica_id).await.unwrap().unwrap();

            let mut light_ready = mut_group.raft_group.advance(ready_group);
            if let Some (commit) = light_ready.commit_index() {
                group_storage.set_commit(commit);
            }

            let messages = light_ready.take_messages();
                        
            
        }
    } 
}
  
// #[tokio::test(flavor = "multi_thread")]
// async fn test_bootstrap_group() {
//     // tracing_subscriber::fmt::try_init().unwrap();
//     let nodes = 3;
//     let groups = 3;
//     let replicas = 3;
//     let mut multi_rafts = vec![];

//     for node_id in 0..nodes {
//         let store_id = node_id;
//         let storage = MultiRaftGroupMemoryStorage::new(node_id, store_id);
//         let group_cfg = GroupConfig {
//             election_tick: 10,
//             heartbeat_tick: 3,
//             tick_interval: 100,
//         };
//         let multi_raft = MultiRaft::new(group_cfg, node_id, store_id, storage);
//         multi_rafts.push(multi_raft);
//     }

//     let mut next_replica = 1;
//     for (_i, multi_raft) in multi_rafts.iter().enumerate() {
//         for group_id in 1..groups + 1 {
//             let mut msg = RaftGroupManagementMessage::default();
//             msg.set_msg_type(RaftGroupManagementMessageType::MsgInitialGroup);
//             msg.initial_voters = (1..replicas + 1).collect();
//             msg.group_id = group_id;
//             msg.replica_id = next_replica;
//             multi_raft.initial_raft_group(msg).await.unwrap();
//         }
//         next_replica += 1;
//     }

//     tokio::time::sleep(Duration::from_secs(1000)).await;
// }