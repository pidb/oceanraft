use std::collections::HashMap;

use tokio::sync::oneshot;
use uuid::Uuid;

use crate::prelude::ConfChangeV2;
use crate::prelude::Entry;
use crate::prelude::MembershipChangeData;
use crate::prelude::RaftGroupManagement;

use super::error::Error;
use super::group::RaftGroupApplyState;
use super::proposal::Proposal;
use super::response::AppWriteResponse;

pub struct WriteData<RES>
where
    RES: AppWriteResponse,
{
    pub group_id: u64,
    pub term: u64,
    pub data: Vec<u8>,
    pub context: Option<Vec<u8>>,
    pub tx: oneshot::Sender<Result<RES, Error>>,
}

#[derive(abomonation_derive::Abomonation, Clone)]
pub struct ReadIndexContext {
    #[unsafe_abomonate_ignore]
    pub uuid: Uuid,

    /// context for user
    #[unsafe_abomonate_ignore]
    pub context:Option<Vec<u8>> ,
}

pub struct ReadIndexData {
    pub group_id: u64,
    pub context: ReadIndexContext,
    pub tx: oneshot::Sender<Result<(), Error>>,
}

pub enum ProposeMessage<RES: AppWriteResponse> {
    WriteData(WriteData<RES>),
    ReadIndexData(ReadIndexData),
    Membership(MembershipChangeData, oneshot::Sender<Result<RES, Error>>),
}

pub enum AdminMessage {
    Group(RaftGroupManagement, oneshot::Sender<Result<(), Error>>),
}

#[allow(unused)]
pub const SUGGEST_MAX_APPLY_BATCH_SIZE: usize = 64 * 1024 * 1024;

#[derive(Debug)]
pub struct ApplyData<RES>
where
    RES: AppWriteResponse,
{
    pub replica_id: u64,
    pub group_id: u64,
    pub term: u64,
    pub commit_index: u64,
    pub commit_term: u64,
    pub entries: Vec<Entry>,
    pub entries_size: usize,
    pub proposals: Vec<Proposal<RES>>,
}

impl<RES> ApplyData<RES>
where
    RES: AppWriteResponse,
{
    pub fn try_batch(&mut self, that: &mut ApplyData<RES>, max_batch_size: usize) -> bool {
        assert_eq!(self.replica_id, that.replica_id);
        assert_eq!(self.group_id, that.group_id);
        assert!(that.term >= self.term);
        assert!(that.commit_index >= self.commit_index);
        assert!(that.commit_term >= self.commit_term);
        if max_batch_size == 0 || self.entries_size + that.entries_size > max_batch_size {
            return false;
        }
        self.term = that.term;
        self.commit_index = that.commit_index;
        self.commit_term = that.commit_term;
        self.entries.append(&mut that.entries);
        self.entries_size += that.entries_size;
        self.proposals.append(&mut that.proposals);
        return true;
    }
}

pub enum ApplyMessage<RES: AppWriteResponse> {
    Apply {
        applys: HashMap<u64, ApplyData<RES>>,
    },
}

#[derive(Debug)]
pub struct ApplyResultMessage {
    pub group_id: u64,
    pub apply_state: RaftGroupApplyState,
}

/// Commit membership change results.
///
/// If proposed change is ConfChange, the ConfChangeV2 is converted
/// from ConfChange. If ConfChangeV2 is used, changes contains multiple
/// requests, otherwise changes contains only one request.
#[derive(Debug)]
pub(crate) struct CommitMembership {
    #[allow(unused)]
    pub(crate) entry_index: u64,
    pub(crate) conf_change: ConfChangeV2,
    pub(crate) change_request: MembershipChangeData,
}

#[derive(Debug)]
pub(crate) enum ApplyCommitMessage {
    None,
    Membership((CommitMembership, oneshot::Sender<Result<(), Error>>)),
}

impl Default for ApplyCommitMessage {
    fn default() -> Self {
        ApplyCommitMessage::None
    }
}