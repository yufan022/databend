// Copyright 2021 Datafuse Labs.
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

use common_meta_api::KVApi;
use common_meta_sled_store::openraft::error::RemoveLearnerError;
use common_meta_types::AppliedState;
use common_meta_types::Cmd;
use common_meta_types::ForwardRequest;
use common_meta_types::ForwardResponse;
use common_meta_types::LogEntry;
use common_meta_types::MetaDataError;
use common_meta_types::MetaDataReadError;
use common_meta_types::MetaOperationError;
use common_meta_types::Node;
use common_meta_types::RaftChangeMembershipError;
use common_meta_types::RaftWriteError;
use common_meta_types::SeqV;
use common_metrics::counter::Count;
use tracing::debug;
use tracing::error;
use tracing::info;

use crate::meta_service::ForwardRequestBody;
use crate::meta_service::JoinRequest;
use crate::meta_service::LeaveRequest;
use crate::meta_service::MetaNode;
use crate::metrics::ProposalPending;

/// The container of APIs of a metasrv leader in a metasrv cluster.
///
/// A meta leader does not imply it is actually the leader granted by the cluster.
/// It just means it believes it is the leader an have not yet perceived there is other newer leader.
pub struct MetaLeader<'a> {
    meta_node: &'a MetaNode,
}

impl<'a> MetaLeader<'a> {
    pub fn new(meta_node: &'a MetaNode) -> MetaLeader {
        MetaLeader { meta_node }
    }

    #[tracing::instrument(level = "debug", skip(self, req), fields(target=%req.forward_to_leader))]
    pub async fn handle_forwardable_req(
        &self,
        req: ForwardRequest,
    ) -> Result<ForwardResponse, MetaOperationError> {
        debug!("handle_forwardable_req: {:?}", req);

        match req.body {
            ForwardRequestBody::Ping => Ok(ForwardResponse::Pong),

            ForwardRequestBody::Join(join_req) => {
                self.join(join_req).await?;
                Ok(ForwardResponse::Join(()))
            }
            ForwardRequestBody::Leave(leave_req) => {
                self.leave(leave_req).await?;
                Ok(ForwardResponse::Leave(()))
            }
            ForwardRequestBody::Write(entry) => {
                let res = self.write(entry.clone()).await?;
                Ok(ForwardResponse::AppliedState(res))
            }

            ForwardRequestBody::GetKV(req) => {
                let sm = self.meta_node.get_state_machine().await;
                let res = sm
                    .get_kv(&req.key)
                    .await
                    .map_err(|meta_err| MetaDataReadError::new("get_kv", "", &meta_err))?;
                Ok(ForwardResponse::GetKV(res))
            }
            ForwardRequestBody::MGetKV(req) => {
                let sm = self.meta_node.get_state_machine().await;
                let res = sm
                    .mget_kv(&req.keys)
                    .await
                    .map_err(|meta_err| MetaDataReadError::new("mget_kv", "", &meta_err))?;
                Ok(ForwardResponse::MGetKV(res))
            }
            ForwardRequestBody::ListKV(req) => {
                let sm = self.meta_node.get_state_machine().await;
                let res = sm
                    .prefix_list_kv(&req.prefix)
                    .await
                    .map_err(|meta_err| MetaDataReadError::new("list_kv", "", &meta_err))?;
                Ok(ForwardResponse::ListKV(res))
            }
        }
    }

    /// Join a new node to the cluster.
    ///
    /// - Adds the node to cluster as a non-voter persistently and starts replication.
    /// - Adds the node to membership to let it become a voter.
    ///
    /// If the node is already in cluster membership, it still returns Ok.
    #[tracing::instrument(level = "debug", skip(self))]
    pub async fn join(&self, req: JoinRequest) -> Result<(), RaftChangeMembershipError> {
        let node_id = req.node_id;
        let endpoint = req.endpoint;
        let grpc_api_addr = req.grpc_api_addr;
        let metrics = self.meta_node.raft.metrics().borrow().clone();
        let membership = metrics.membership_config.membership.clone();

        if membership.contains(&node_id) {
            return Ok(());
        }

        // safe unwrap: if the first config is None, panic is the expected behavior here.
        let mut membership = membership.get_ith_config(0).unwrap().clone();
        membership.insert(node_id);
        let ent = LogEntry {
            txid: None,
            time_ms: None,
            cmd: Cmd::AddNode {
                node_id,
                node: Node {
                    name: node_id.to_string(),
                    endpoint,
                    grpc_api_addr: Some(grpc_api_addr),
                },
            },
        };
        self.write(ent).await?;

        self.meta_node
            .raft
            .change_membership(membership, false)
            .await?;
        Ok(())
    }

    /// A node leave the cluster.
    ///
    /// - Remove the node from membership.
    /// - Stop replication.
    /// - Remove the node from cluster.
    ///
    /// If the node is not in cluster membership, it still returns Ok.
    #[tracing::instrument(level = "debug", skip(self))]
    pub async fn leave(&self, req: LeaveRequest) -> Result<(), MetaOperationError> {
        let node_id = req.node_id;

        let can_res =
            self.meta_node.can_leave(node_id).await.map_err(|e| {
                MetaDataError::ReadError(MetaDataReadError::new("can_leave()", "", &e))
            })?;

        if let Err(e) = can_res {
            info!("no need to leave: {}", e);
            return Ok(());
        }

        // safe unwrap: if the first config is None, panic is the expected behavior here.
        let membership = {
            let sm = self.meta_node.sto.get_state_machine().await;
            let m = sm.get_membership().map_err(|e| {
                MetaDataError::ReadError(MetaDataReadError::new("get_membership()", "", &e))
            })?;

            // Safe unwrap(): can_leave() assert that m is not None.
            m.unwrap().membership
        };

        // 1. Remove it from membership if needed.
        if membership.contains(&node_id) {
            let mut config0 = membership.get_ith_config(0).unwrap().clone();
            config0.remove(&node_id);

            self.meta_node.raft.change_membership(config0, true).await?;
        }

        // 2. Stop replication
        let res = self.meta_node.raft.remove_learner(node_id).await;
        if let Err(e) = res {
            match e {
                RemoveLearnerError::ForwardToLeader(e) => {
                    return Err(MetaOperationError::ForwardToLeader(e));
                }
                RemoveLearnerError::NotLearner(_e) => {
                    error!("Node to leave the cluster is not a learner: {}", node_id);
                }
                RemoveLearnerError::NotExists(_e) => {
                    info!("Node to leave the cluster does not exists: {}", node_id);
                }
                RemoveLearnerError::Fatal(e) => {
                    return Err(MetaOperationError::DataError(MetaDataError::WriteError(e)));
                }
            }
        }

        // 3. Remove node info
        let ent = LogEntry {
            txid: None,
            time_ms: None,
            cmd: Cmd::RemoveNode { node_id },
        };
        self.write(ent).await?;

        Ok(())
    }

    /// Write a log through local raft node and return the states before and after applying the log.
    ///
    /// If the raft node is not a leader, it returns MetaRaftError::ForwardToLeader.
    #[tracing::instrument(level = "debug", skip(self, entry))]
    pub async fn write(&self, mut entry: LogEntry) -> Result<AppliedState, RaftWriteError> {
        // Add consistent clock time to log entry.
        entry.time_ms = Some(SeqV::<()>::now_ms());

        // report metrics
        let _guard = ProposalPending::guard();

        info!("write LogEntry: {}", entry);
        let write_res = self.meta_node.raft.client_write(entry).await;
        if let Ok(ok) = &write_res {
            info!(
                "raft.client_write res ok: log_id: {}, data: {}, membership: {:?}",
                ok.log_id, ok.data, ok.membership
            );
        }
        if let Err(err) = &write_res {
            info!("raft.client_write res err: {:?}", err);
        }

        match write_res {
            Ok(resp) => Ok(resp.data),
            Err(cli_write_err) => Err(RaftWriteError::from_raft_err(cli_write_err)),
        }
    }
}
