use futures::Future;
use oceanraft::multiraft::storage::ApplyWriteBatch;
use oceanraft::multiraft::storage::StateMachineStore;
use oceanraft::multiraft::Apply;
use oceanraft::multiraft::GroupState;
use oceanraft::multiraft::StateMachine;
use oceanraft::multiraft::WriteResponse;
use oceanraft::prelude::StoreData;
use tokio::sync::mpsc::Sender;

#[derive(Clone)]
pub struct FixtureStateMachine<R>
where
    R: WriteResponse,
{
    kv_store: StateMachineStore<R>,
    tx: Sender<Vec<Apply<StoreData, R>>>,
}

impl<R> FixtureStateMachine<R>
where
    R: WriteResponse,
{
    pub fn new(kv_store: StateMachineStore<R>, tx: Sender<Vec<Apply<StoreData, R>>>) -> Self {
        Self { kv_store, tx }
    }
}

impl<R> StateMachine<StoreData, R> for FixtureStateMachine<R>
where
    R: WriteResponse,
{
    type ApplyFuture<'life0> = impl Future<Output = ()> + 'life0
    where
        Self: 'life0;
    fn apply<'life0>(
        &'life0 self,
        group_id: u64,
        _state: &GroupState,
        mut applys: Vec<Apply<StoreData, R>>,
    ) -> Self::ApplyFuture<'life0> {
        let tx = self.tx.clone();
        async move {
            let mut batch = self.kv_store.write_batch_for_apply(group_id);
            for apply in applys.iter_mut() {
                match apply {
                    Apply::NoOp(noop) => {
                        batch.set_applied_index(noop.index);
                        batch.set_applied_term(noop.term);
                    }
                    Apply::Normal(normal) => {
                        batch.put_data(&normal.data);
                        batch.set_applied_index(normal.index);
                        batch.set_applied_term(normal.term);
                    }
                    Apply::Membership(membership) => {
                        membership.done().await.unwrap();
                        batch.set_applied_index(membership.index);
                        batch.set_applied_term(membership.term);
                    }
                }
            }
            self.kv_store.write_apply_bath(group_id, batch).unwrap();

            tx.send(applys).await;
        }
    }
}

// #[derive(Clone)]
// pub struct FixtureMultiStateMachine<R>
// where
//     R: WriteResponse,
// {
//     kv_store: KVStore<R>,
//     machines: Arc<RwLock<HashMap<u64, FixtureStateMachine<R>>>>,
//     rxs: Arc<RwLock<HashMap<u64, Arc<Mutex<Receiver<Apply<StoreData, R>>>>>>>,
// }

// impl<R> FixtureMultiStateMachine<R>
// where
//     R: WriteResponse,
// {
//     pub fn new(kv_store: KVStore<R>) -> Self {
//         Self {
//             rxs: Default::default(),
//             machines: Default::default(),
//             kv_store,
//         }
//     }

//     pub fn get_rx(&self, group_id: u64) -> Arc<Mutex<Receiver<Apply<StoreData, R>>>> {
//         let lock = self.rxs.read().unwrap();
//         lock.get(&group_id).unwrap().clone()
//     }
// }

// impl<R> MultiStateMachine<StoreData, R> for FixtureMultiStateMachine<R>
// where
//     R: WriteResponse,
// {
//     type E = FixtureWriteDataError;
//     type S = FixtureStateMachine<R>;

//     fn create_state_machine(&self, group_id: u64) -> Result<Self::S, Self::E> {
//         {
//             let rl = self.machines.read().unwrap();
//             if let Some(machine) = rl.get(&group_id) {
//                 return Ok(machine.clone());
//             }
//         }

//         let (tx, rx) = channel(100);
//         let machine = FixtureStateMachine {
//             group_id,
//             tx,
//             kv_store: self.kv_store.clone(),
//         };

//         {
//             let mut wl = self.machines.write().unwrap();
//             wl.insert(group_id, machine.clone());
//         }

//         let mut wl = self.rxs.write().unwrap();
//         assert_eq!(
//             wl.insert(group_id, Arc::new(Mutex::new(rx))).is_none(),
//             true
//         );

//         Ok(machine)
//     }
// }
