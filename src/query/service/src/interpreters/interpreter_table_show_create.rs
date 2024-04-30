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

use databend_common_catalog::table::Table;
use databend_common_exception::ErrorCode;
use databend_common_exception::Result;
use databend_common_expression::types::DataType;
use databend_common_expression::BlockEntry;
use databend_common_expression::ComputedExpr;
use databend_common_expression::DataBlock;
use databend_common_expression::Scalar;
use databend_common_expression::Value;
use databend_common_sql::plans::ShowCreateTablePlan;
use databend_common_storages_stream::stream_table::StreamTable;
use databend_common_storages_stream::stream_table::STREAM_ENGINE;
use databend_common_storages_view::view_table::QUERY;
use databend_common_storages_view::view_table::VIEW_ENGINE;
use databend_storages_common_table_meta::table::is_internal_opt_key;
use databend_storages_common_table_meta::table::OPT_KEY_STORAGE_PREFIX;
use databend_storages_common_table_meta::table::OPT_KEY_TABLE_ATTACHED_DATA_URI;
use databend_storages_common_table_meta::table::OPT_KEY_TABLE_ATTACHED_READ_ONLY;
use log::debug;

use crate::interpreters::Interpreter;
use crate::pipelines::PipelineBuildResult;
use crate::sessions::QueryContext;
use crate::sessions::TableContext;

pub struct ShowCreateTableInterpreter {
    ctx: Arc<QueryContext>,
    plan: ShowCreateTablePlan,
}

impl ShowCreateTableInterpreter {
    pub fn try_create(ctx: Arc<QueryContext>, plan: ShowCreateTablePlan) -> Result<Self> {
        Ok(ShowCreateTableInterpreter { ctx, plan })
    }
}

#[async_trait::async_trait]
impl Interpreter for ShowCreateTableInterpreter {
    fn name(&self) -> &str {
        "ShowCreateTableInterpreter"
    }

    #[async_backtrace::framed]
    async fn execute2(&self) -> Result<PipelineBuildResult> {
        let tenant = self.ctx.get_tenant();
        let catalog = self.ctx.get_catalog(self.plan.catalog.as_str()).await?;

        let table = catalog
            .get_table(tenant.as_str(), &self.plan.database, &self.plan.table)
            .await?;

        match table.engine() {
            STREAM_ENGINE => self.show_create_stream(table.as_ref()),
            VIEW_ENGINE => self.show_create_view(table.as_ref()),
            _ => match table.options().get(OPT_KEY_STORAGE_PREFIX) {
                Some(_) => self.show_attach_table(table.as_ref()),
                None => self.show_create_table(table.as_ref()),
            },
        }
    }
}

impl ShowCreateTableInterpreter {
    fn show_create_table(&self, table: &dyn Table) -> Result<PipelineBuildResult> {
        let name = table.name();
        let engine = table.engine();
        let schema = table.schema();
        let field_comments = table.field_comments();
        let n_fields = schema.fields().len();

        let mut table_create_sql = format!("CREATE TABLE `{}` (\n", name);
        if table.options().contains_key("TRANSIENT") {
            table_create_sql = format!("CREATE TRANSIENT TABLE `{}` (\n", name)
        }

        // Append columns.
        {
            let mut columns = vec![];
            for (idx, field) in schema.fields().iter().enumerate() {
                let nullable = if field.is_nullable() {
                    " NULL".to_string()
                } else {
                    " NOT NULL".to_string()
                };
                let default_expr = match field.default_expr() {
                    Some(expr) => {
                        format!(" DEFAULT {expr}")
                    }
                    None => "".to_string(),
                };
                let computed_expr = match field.computed_expr() {
                    Some(ComputedExpr::Virtual(expr)) => {
                        format!(" AS ({expr}) VIRTUAL")
                    }
                    Some(ComputedExpr::Stored(expr)) => {
                        format!(" AS ({expr}) STORED")
                    }
                    _ => "".to_string(),
                };
                // compatibility: creating table in the old planner will not have `fields_comments`
                let comment = if field_comments.len() == n_fields && !field_comments[idx].is_empty()
                {
                    // make the display more readable.
                    format!(
                        " COMMENT '{}'",
                        &field_comments[idx].as_str().replace('\'', "\\'")
                    )
                } else {
                    "".to_string()
                };
                let column = format!(
                    "  `{}` {}{}{}{}{}",
                    field.name(),
                    field.data_type().remove_recursive_nullable().sql_name(),
                    nullable,
                    default_expr,
                    computed_expr,
                    comment
                );

                columns.push(column);
            }
            // Format is:
            //  (
            //      x,
            //      y
            //  )
            let columns_str = format!("{}\n", columns.join(",\n"));
            table_create_sql.push_str(&columns_str);
        }

        let table_engine = format!(") ENGINE={}", engine);
        table_create_sql.push_str(table_engine.as_str());

        let table_info = table.get_table_info();
        if let Some((_, cluster_keys_str)) = table_info.meta.cluster_key() {
            table_create_sql.push_str(format!(" CLUSTER BY {}", cluster_keys_str).as_str());
        }

        let settings = self.ctx.get_settings();
        let hide_options_in_show_create_table = settings
            .get_hide_options_in_show_create_table()
            .unwrap_or(false);

        if !hide_options_in_show_create_table || engine == "ICEBERG" || engine == "DELTA" {
            table_create_sql.push_str({
                let mut opts = table_info.options().iter().collect::<Vec<_>>();
                opts.sort_by_key(|(k, _)| *k);
                opts.iter()
                    .filter(|(k, _)| !is_internal_opt_key(k))
                    .map(|(k, v)| format!(" {}='{}'", k.to_uppercase(), v))
                    .collect::<Vec<_>>()
                    .join("")
                    .as_str()
            });
        }

        if !table_info.meta.comment.is_empty() {
            table_create_sql.push_str(format!(" COMMENT = '{}'", table_info.meta.comment).as_str());
        }

        let block = DataBlock::new(
            vec![
                BlockEntry::new(
                    DataType::String,
                    Value::Scalar(Scalar::String(name.to_string())),
                ),
                BlockEntry::new(
                    DataType::String,
                    Value::Scalar(Scalar::String(table_create_sql)),
                ),
            ],
            1,
        );
        debug!("Show create table executor result: {:?}", block);

        PipelineBuildResult::from_blocks(vec![block])
    }

    fn show_create_view(&self, table: &dyn Table) -> Result<PipelineBuildResult> {
        let name = table.name();
        if let Some(query) = table.options().get(QUERY) {
            let view_create_sql = format!(
                "CREATE VIEW `{}`.`{}` AS {}",
                &self.plan.database, name, query
            );
            let block = DataBlock::new(
                vec![
                    BlockEntry::new(
                        DataType::String,
                        Value::Scalar(Scalar::String(name.to_string())),
                    ),
                    BlockEntry::new(
                        DataType::String,
                        Value::Scalar(Scalar::String(view_create_sql)),
                    ),
                ],
                1,
            );
            debug!("Show create view executor result: {:?}", block);

            PipelineBuildResult::from_blocks(vec![block])
        } else {
            Err(ErrorCode::Internal(
                "Logical error, View Table must have a SelectQuery inside.",
            ))
        }
    }

    fn show_create_stream(&self, table: &dyn Table) -> Result<PipelineBuildResult> {
        let stream_table = StreamTable::try_from_table(table)?;
        let mut create_sql = format!(
            "CREATE STREAM `{}` ON TABLE `{}`.`{}`",
            stream_table.name(),
            stream_table.source_table_database(),
            stream_table.source_table_name()
        );

        let comment = stream_table.get_table_info().meta.comment.clone();
        if !comment.is_empty() {
            create_sql.push_str(format!(" COMMENT = '{}'", comment).as_str());
        }
        let block = DataBlock::new(
            vec![
                BlockEntry::new(
                    DataType::String,
                    Value::Scalar(Scalar::String(stream_table.name().to_string())),
                ),
                BlockEntry::new(DataType::String, Value::Scalar(Scalar::String(create_sql))),
            ],
            1,
        );
        PipelineBuildResult::from_blocks(vec![block])
    }

    fn show_attach_table(&self, table: &dyn Table) -> Result<PipelineBuildResult> {
        let name = table.name();
        // TODO table that attached before this PR, could not show location properly
        let location_not_available = "N/A".to_string();
        let table_data_location = table
            .options()
            .get(OPT_KEY_TABLE_ATTACHED_DATA_URI)
            .unwrap_or(&location_not_available);

        let mut ddl = format!(
            "ATTACH TABLE `{}`.`{}` {}",
            &self.plan.database, name, table_data_location,
        );

        if table
            .options()
            .contains_key(OPT_KEY_TABLE_ATTACHED_READ_ONLY)
        {
            ddl.push_str(" READ_ONLY")
        }

        let block = DataBlock::new(
            vec![
                BlockEntry::new(
                    DataType::String,
                    Value::Scalar(Scalar::String(name.to_string())),
                ),
                BlockEntry::new(DataType::String, Value::Scalar(Scalar::String(ddl))),
            ],
            1,
        );
        PipelineBuildResult::from_blocks(vec![block])
    }
}
