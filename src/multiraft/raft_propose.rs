use raft::StateRole;
use prost::Message;
use tokio::sync::oneshot;

use crate::proto::AppWriteRequest;
use crate::proto::AppReadIndexRequest;
use crate::storage::RaftStorage;

use super::error::Error;
use super::error::ProposalError;
use super::error::RaftError;
use super::multiraft::RaftGroup;
use super::proposal::Proposal;
use super::proposal::ReadIndexProposal;

impl<RS> RaftGroup<RS>
where
    RS: RaftStorage,
{
    #[inline]
    pub fn is_leader(&self) -> bool {
        self.raft_group.raft.state == StateRole::Leader
    }

    #[inline]
    pub fn term(&self) -> u64 {
        self.raft_group.raft.term
    }

    #[inline]
    pub fn last_index(&self) -> u64 {
        self.raft_group.raft.raft_log.last_index()
    }

    fn write_pre_propose(&mut self, request: &AppWriteRequest) -> Result<(), Error>
    where
        RS: RaftStorage,
    {
        if request.data.is_empty() {
            return Err(Error::BadParameter(format!("write request data is empty")));
        }

        if !self.is_leader() {
            return Err(Error::Raft(RaftError::NotLeader(
                self.group_id,
                self.replica_id,
                self.raft_group.raft.leader_id,
            )));
        }

        if request.term != 0 && self.term() > request.term {
            return Err(Error::Proposal(ProposalError::Stale(request.term)));
        }

        Ok(())
    }

    pub fn write_propose(
        &mut self,
        request: AppWriteRequest,
        tx: oneshot::Sender<Result<(), Error>>,
    ) {
        if let Err(err) = self.write_pre_propose(&request) {
            tx.send(Err(err)).unwrap();
            return;
        }
        let term = self.term();

        // propose to raft gorup
        let expected_next_index = self.last_index() + 1;

        if let Err(err) = self.raft_group.propose(request.context, request.data) {
            tx.send(Err(Error::Proposal(ProposalError::Other(Box::new(err)))))
                .unwrap();
            return;
        }

        let index = self.last_index() + 1;
        if expected_next_index != index {
            tx.send(Err(Error::Proposal(ProposalError::Unexpected(index))))
                .unwrap();
            return;
        }

        let proposal = Proposal {
            index,
            term,
            is_conf_change: false,
            tx: Some(tx),
        };

        self.proposals.push(proposal).unwrap();
    }

    pub fn read_index_propose(&mut self,
            request: AppReadIndexRequest,
        tx: oneshot::Sender<Result<(), Error>>
    ) {
        let uuid = uuid::Uuid::new_v4();
        let term = self.term();
        let read_context = match request.context {
            None => vec![],
            Some(ctx) => ctx.encode_length_delimited_to_vec(),
        };

        self.raft_group.read_index(read_context);

        let proposal = ReadIndexProposal {
            uuid,
            read_index: None,
            context: None,
            tx: Some(tx),
        };
    }
}
