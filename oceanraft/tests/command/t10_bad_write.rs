use oceanraft::multiraft::Error;
use oceanraft::multiraft::WriteError;
use oceanraft::prelude::StoreData;
use oceanraft::util::TaskGroup;

use crate::fixtures::init_default_ut_tracing;
use crate::fixtures::ClusterBuilder;
use crate::fixtures::MakeGroupPlan;
use crate::fixtures::RockStorageEnv;

/// The test consensus group does not have a leader or the leader is
/// submitting a proposal during an election.
#[async_entry::test(
    flavor = "multi_thread",
    init = "init_default_ut_tracing()",
    tracing_span = "debug"
)]
async fn test_no_leader() {
    let nodes = 3;
    let task_group = TaskGroup::new();
    let rockstore_env = RockStorageEnv::<()>::new(nodes);
    let mut cluster = ClusterBuilder::new(nodes)
        .election_ticks(2)
        .task_group(task_group.clone())
        .kv_stores(rockstore_env.rock_kv_stores.clone())
        .storages(rockstore_env.storages.clone())
        .build()
        .await;

    let mut plan = MakeGroupPlan {
        group_id: 1,
        first_node_id: 1,
        replica_nums: 3,
    };
    let _ = cluster.make_group(&mut plan).await.unwrap();

    // all replicas should no elected.
    for i in 0..3 {
        let node_id = i + 1;
        if let Ok(ev) = cluster.wait_leader_elect_event(node_id).await {
            panic!("expected no leader elected, got {:?}", ev);
        }
    }

    for i in 0..3 {
        let node_id = i + 1;
        let data = StoreData {
            key: "key".to_string(),
            value: "data".as_bytes().to_vec(),
        };
        let expected_err = Error::Write(WriteError::NotLeader {
            node_id,
            group_id: plan.group_id,
            replica_id: i + 1,
        });

        match cluster.write_command(node_id, plan.group_id, data) {
            Ok(res) => panic!("expected {:?}, got {:?}", expected_err, res),
            Err(err) => assert_eq!(expected_err.to_string(), err.to_string()),
        }
    }

    rockstore_env.destory();
    // cluster.stop().await;
}

//

/// The test consensus group does not have a leader or the leader is
/// submitting a proposal during an election.
#[async_entry::test(
    flavor = "multi_thread",
    init = "init_default_ut_tracing()",
    tracing_span = "debug"
)]
async fn test_bad_group() {
    let nodes = 3;
    let task_group = TaskGroup::new();
    let rockstore_env = RockStorageEnv::<()>::new(nodes);
    let mut cluster = ClusterBuilder::new(nodes)
        .election_ticks(2)
        .task_group(task_group.clone())
        .kv_stores(rockstore_env.rock_kv_stores.clone())
        .storages(rockstore_env.storages.clone())
        .build()
        .await;
    let mut plan = MakeGroupPlan {
        group_id: 1,
        first_node_id: 1,
        replica_nums: 3,
    };

    // now, trigger leader elect and it's should became leader.
    let _ = cluster.make_group(&mut plan).await.unwrap();
    cluster.campaign_group(1, plan.group_id).await;
    let _ = cluster.wait_leader_elect_event(1).await.unwrap();

    for i in 1..3 {
        let node_id = i + 1;

        let data = StoreData {
            key: "key".to_string(),
            value: "data".as_bytes().to_vec(),
        };
        let expected_err = Error::Write(WriteError::NotLeader {
            node_id,
            group_id: plan.group_id,
            replica_id: i + 1,
        });
        match cluster.write_command(node_id, plan.group_id, data) {
            Err(err) => assert_eq!(expected_err.to_string(), err.to_string()),
            Ok(rx) => match rx.await.unwrap() {
                Ok(res) => panic!("expected {:?}, got {:?}", expected_err, res),
                Err(err) => assert_eq!(expected_err.to_string(), err.to_string()),
            },
        }
    }
    // cluster.stop().await;
}
