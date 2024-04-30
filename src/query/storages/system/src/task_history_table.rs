// Copyright 2021 Datafuse Labs
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

use std::sync::Arc;

use databend_common_catalog::plan::PushDownInfo;
use databend_common_catalog::table::Table;
use databend_common_catalog::table_context::TableContext;
use databend_common_cloud_control::client_config::build_client_config;
use databend_common_cloud_control::cloud_api::CloudControlApiProvider;
use databend_common_cloud_control::pb::ShowTaskRunsRequest;
use databend_common_cloud_control::pb::TaskRun;
use databend_common_cloud_control::task_client::make_request;
use databend_common_config::GlobalConfig;
use databend_common_exception::ErrorCode;
use databend_common_exception::Result;
use databend_common_expression::infer_table_schema;
use databend_common_expression::types::Int32Type;
use databend_common_expression::types::Int64Type;
use databend_common_expression::types::StringType;
use databend_common_expression::types::TimestampType;
use databend_common_expression::types::UInt64Type;
use databend_common_expression::types::VariantType;
use databend_common_expression::DataBlock;
use databend_common_expression::FromData;
use databend_common_meta_app::schema::TableIdent;
use databend_common_meta_app::schema::TableInfo;
use databend_common_meta_app::schema::TableMeta;
use databend_common_sql::plans::task_run_schema;

use crate::table::AsyncOneBlockSystemTable;
use crate::table::AsyncSystemTable;

pub fn parse_task_runs_to_datablock(task_runs: Vec<TaskRun>) -> Result<DataBlock> {
    let mut name: Vec<String> = Vec::with_capacity(task_runs.len());
    let mut id: Vec<u64> = Vec::with_capacity(task_runs.len());
    let mut owner: Vec<String> = Vec::with_capacity(task_runs.len());
    let mut definition: Vec<String> = Vec::with_capacity(task_runs.len());
    let mut condition_text: Vec<String> = Vec::with_capacity(task_runs.len());
    let mut comment: Vec<Option<String>> = Vec::with_capacity(task_runs.len());
    let mut schedule: Vec<Option<String>> = Vec::with_capacity(task_runs.len());
    let mut warehouse: Vec<Option<String>> = Vec::with_capacity(task_runs.len());
    let mut state: Vec<String> = Vec::with_capacity(task_runs.len());
    let mut exception_text: Vec<Option<String>> = Vec::with_capacity(task_runs.len());
    let mut exception_code: Vec<i64> = Vec::with_capacity(task_runs.len());
    let mut run_id: Vec<String> = Vec::with_capacity(task_runs.len());
    let mut query_id: Vec<String> = Vec::with_capacity(task_runs.len());
    let mut attempt_number: Vec<i32> = Vec::with_capacity(task_runs.len());
    let mut scheduled_time: Vec<i64> = Vec::with_capacity(task_runs.len());
    let mut completed_time: Vec<Option<i64>> = Vec::with_capacity(task_runs.len());
    let mut root_task_id: Vec<String> = Vec::with_capacity(task_runs.len());
    let mut session_params: Vec<Option<Vec<u8>>> = Vec::with_capacity(task_runs.len());

    for task_run in task_runs {
        let tr: databend_common_cloud_control::task_utils::TaskRun = task_run.try_into()?;
        name.push(tr.task_name);
        id.push(tr.task_id);
        owner.push(tr.owner);
        comment.push(tr.comment);
        schedule.push(tr.schedule_options);
        warehouse.push(tr.warehouse_options.and_then(|s| s.warehouse));
        state.push(tr.state.to_string());
        exception_code.push(tr.error_code);
        exception_text.push(tr.error_message);
        definition.push(tr.query_text);
        condition_text.push(tr.condition_text);
        run_id.push(tr.run_id);
        query_id.push(tr.query_id);
        attempt_number.push(tr.attempt_number);
        completed_time.push(tr.completed_at.map(|t| t.timestamp_micros()));
        scheduled_time.push(tr.scheduled_at.timestamp_micros());
        root_task_id.push(tr.root_task_id);
        let serialized_params = serde_json::to_vec(&tr.session_params).unwrap();
        session_params.push(Some(serialized_params));
    }
    Ok(DataBlock::new_from_columns(vec![
        StringType::from_data(name),
        UInt64Type::from_data(id),
        StringType::from_data(owner),
        StringType::from_opt_data(comment),
        StringType::from_opt_data(schedule),
        StringType::from_opt_data(warehouse),
        StringType::from_data(state),
        StringType::from_data(definition),
        StringType::from_data(condition_text),
        StringType::from_data(run_id),
        StringType::from_data(query_id),
        Int64Type::from_data(exception_code),
        StringType::from_opt_data(exception_text),
        Int32Type::from_data(attempt_number),
        TimestampType::from_opt_data(completed_time),
        TimestampType::from_data(scheduled_time),
        StringType::from_data(root_task_id),
        VariantType::from_opt_data(session_params),
    ]))
}

pub struct TaskHistoryTable {
    table_info: TableInfo,
}

#[async_trait::async_trait]
impl AsyncSystemTable for TaskHistoryTable {
    const NAME: &'static str = "system.task_history";

    fn get_table_info(&self) -> &TableInfo {
        &self.table_info
    }

    #[async_backtrace::framed]
    async fn get_full_data(
        &self,
        ctx: Arc<dyn TableContext>,
        _push_downs: Option<PushDownInfo>,
    ) -> Result<DataBlock> {
        let config = GlobalConfig::instance();
        if config.query.cloud_control_grpc_server_address.is_none() {
            return Err(ErrorCode::CloudControlNotEnabled(
                "cannot view system.task_history table without cloud control enabled, please set cloud_control_grpc_server_address in config",
            ));
        }

        let tenant = ctx.get_tenant();
        let query_id = ctx.get_id();
        let user = ctx.get_current_user()?.identity().to_string();
        let available_roles = ctx.get_available_roles().await?;
        let req = ShowTaskRunsRequest {
            tenant_id: tenant.clone(),
            scheduled_time_start: "".to_string(),
            scheduled_time_end: "".to_string(),
            task_name: "".to_string(),
            result_limit: 10000, // TODO: use plan.limit pushdown
            error_only: false,
            owners: available_roles
                .into_iter()
                .map(|x| x.identity().to_string())
                .collect(),
            task_ids: vec![],
        };

        let cloud_api = CloudControlApiProvider::instance();
        let task_client = cloud_api.get_task_client();
        let config = build_client_config(tenant, user, query_id);
        let req = make_request(req, config);

        let resp = task_client.show_task_runs(req).await?;
        let trs = resp.task_runs;

        parse_task_runs_to_datablock(trs)
    }
}

impl TaskHistoryTable {
    pub fn create(table_id: u64) -> Arc<dyn Table> {
        let schema = infer_table_schema(&task_run_schema())
            .expect("failed to parse task history table schema");

        let table_info = TableInfo {
            desc: "'system'.'task_history'".to_string(),
            name: "task_history".to_string(),
            ident: TableIdent::new(table_id, 0),
            meta: TableMeta {
                schema,
                engine: "SystemTaskHistory".to_string(),

                ..Default::default()
            },
            ..Default::default()
        };

        AsyncOneBlockSystemTable::create(Self { table_info })
    }
}
