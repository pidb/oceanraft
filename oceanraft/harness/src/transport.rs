use std::collections::hash_map::HashMap;
use std::marker::PhantomData;
use std::sync::Arc;

use oceanraft::util::Stopper;
use oceanraft::util::TaskGroup;
use tracing::info;

use tokio::sync::mpsc::channel;
use tokio::sync::mpsc::Receiver;
use tokio::sync::mpsc::Sender;
use tokio::sync::oneshot;
use tokio::sync::watch;
use tokio::sync::RwLock;
use tokio::task::JoinHandle;

use oceanraft::multiraft::transport::RaftMessageDispatch;
use oceanraft::multiraft::transport::Transport;
use oceanraft::multiraft::Error;
use oceanraft::multiraft::TransportError;
use oceanraft::prelude::RaftMessage;
use oceanraft::prelude::RaftMessageResponse;

struct LocalServer<M: RaftMessageDispatch> {
    tx: Sender<(
        RaftMessage,
        oneshot::Sender<Result<RaftMessageResponse, Error>>,
    )>,
    _m1: PhantomData<M>,
}

impl<RD: RaftMessageDispatch> LocalServer<RD> {
    /// Spawn a server to accepct request.
    #[tracing::instrument(name = "LocalServer::spawn", skip(rx, dispatcher, task_group))]
    fn spawn(
        node_id: u64,
        addr: &str,
        dispatcher: RD,
        mut rx: Receiver<(
            RaftMessage,
            oneshot::Sender<Result<RaftMessageResponse, Error>>,
        )>,
        task_group: &TaskGroup,
    ) -> JoinHandle<()> {
        let addr = addr.to_string().clone();
        let mut stopper = task_group.stopper();
        let main_loop = async move {
            info!("the node ({}) of server listen at {}", node_id, addr);
            loop {
                tokio::select! {
                    Some((msg, tx)) = rx.recv() => {
                        info!("recv msg {:?} and dispatch it", msg);
                        let res = dispatcher.dispatch(msg).await;
                        tx.send(res).unwrap();
                    },
                    _ = &mut stopper => {
                        println!("server stop");
                        break
                    },
                }
            }
        };

        task_group.spawn(main_loop)
    }
}

#[derive(Clone)]
pub struct LocalTransport<M: RaftMessageDispatch> {
    task_group: TaskGroup,
    servers: Arc<RwLock<HashMap<u64, LocalServer<M>>>>,
}

impl<M: RaftMessageDispatch> LocalTransport<M> {
    pub fn new() -> Self {
        Self {
            task_group: TaskGroup::new(),
            servers: Default::default(),
        }
    }
}

impl<RD: RaftMessageDispatch> LocalTransport<RD> {
    #[tracing::instrument(name = "LocalTransport::listen", skip(self, dispatcher))]
    pub async fn listen<'life0>(
        &'life0 self,
        node_id: u64,
        addr: &'life0 str,
        dispatcher: RD,
    ) -> Result<(), Error> {
        // check exists
        {
            let rl = self.servers.write().await;
            if rl.contains_key(&node_id) {
                return Err(Error::Transport(TransportError::ServerAlreadyExists(
                    node_id,
                )));
            }
        }

        // create server

        let (tx, rx) = channel(1);
        let local_server = LocalServer {
            tx,
            _m1: PhantomData,
        };

        let mut wl = self.servers.write().await;
        wl.insert(node_id, local_server);

        // spawn server to accepct request
        let _ = LocalServer::spawn(node_id, addr, dispatcher, rx, &self.task_group);

        Ok(())
    }

    #[tracing::instrument(name = "LocalTransport::stop_all", skip(self))]
    pub async fn stop_all(&self) -> Result<(), Error> {
        self.task_group.stop();
        self.task_group.joinner().await;
        Ok(())
    }
}

impl<RD> Transport for LocalTransport<RD>
where
    RD: RaftMessageDispatch,
{
    #[tracing::instrument(name = "LocalTransport::send", skip(self, msg))]
    fn send(&self, msg: RaftMessage) -> Result<(), Error> {
        let (from_node, to_node) = (msg.from_node, msg.to_node);

        info!("{} -> {}", from_node, to_node);
        let servers = self.servers.clone();

        // get client
        let send_fn = async move {
            // get server by to
            let rl = servers.read().await;
            if !rl.contains_key(&to_node) {
                return Err(Error::Transport(TransportError::ServerNodeFound(to_node)));
            }

            let (tx, rx) = oneshot::channel();
            // send reqeust
            let local_server = rl.get(&to_node).unwrap();
            local_server.tx.send((msg, tx)).await.unwrap();

            // and receive response
            if let Ok(res) = rx.await {
                info!("recv response ok()");
                res
            } else {
                info!("recv response error()");
                Err(Error::Transport(TransportError::Server(format!(
                    "server ({}) stopped",
                    to_node
                ))))
            }
        };
        tokio::spawn(send_fn);
        Ok(())
    }
}
