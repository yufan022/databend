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

use std::any::Any;
use std::collections::BTreeMap;
use std::sync::Arc;

use databend_common_ast::ast::FileLocation;
use databend_common_ast::ast::UriLocation;
use databend_common_catalog::plan::DataSourcePlan;
use databend_common_catalog::plan::PartStatistics;
use databend_common_catalog::plan::Partitions;
use databend_common_catalog::plan::PushDownInfo;
use databend_common_catalog::table::Table;
use databend_common_catalog::table_args::TableArgs;
use databend_common_exception::ErrorCode;
use databend_common_exception::Result;
use databend_common_expression::types::BooleanType;
use databend_common_expression::types::NumberDataType;
use databend_common_expression::types::StringType;
use databend_common_expression::types::UInt64Type;
use databend_common_expression::DataBlock;
use databend_common_expression::FromData;
use databend_common_expression::TableDataType;
use databend_common_expression::TableField;
use databend_common_expression::TableSchema;
use databend_common_expression::TableSchemaRefExt;
use databend_common_meta_app::principal::StageFileFormatType;
use databend_common_meta_app::schema::TableIdent;
use databend_common_meta_app::schema::TableInfo;
use databend_common_meta_app::schema::TableMeta;
use databend_common_pipeline_core::processors::ProcessorPtr;
use databend_common_pipeline_core::Pipeline;
use databend_common_pipeline_sources::AsyncSource;
use databend_common_pipeline_sources::AsyncSourcer;
use databend_common_sql::binder::resolve_file_location;
use databend_common_storage::init_stage_operator;
use databend_common_storage::read_parquet_schema_async;
use databend_common_storage::read_parquet_schema_async_rs;
use databend_common_storage::StageFilesInfo;
use opendal::Scheme;

use crate::pipelines::processors::OutputPort;
use crate::sessions::TableContext;
use crate::table_functions::infer_schema::table_args::InferSchemaArgsParsed;
use crate::table_functions::TableFunction;

const INFER_SCHEMA: &str = "infer_schema";

pub struct InferSchemaTable {
    table_info: TableInfo,
    args_parsed: InferSchemaArgsParsed,
    table_args: TableArgs,
}

impl InferSchemaTable {
    pub fn create(
        database_name: &str,
        table_func_name: &str,
        table_id: u64,
        table_args: TableArgs,
    ) -> Result<Arc<dyn TableFunction>> {
        let args_parsed = InferSchemaArgsParsed::parse(&table_args)?;
        let table_info = TableInfo {
            ident: TableIdent::new(table_id, 0),
            desc: format!("'{}'.'{}'", database_name, table_func_name),
            name: table_func_name.to_string(),
            meta: TableMeta {
                schema: Self::schema(),
                engine: INFER_SCHEMA.to_owned(),
                ..Default::default()
            },
            ..Default::default()
        };

        Ok(Arc::new(Self {
            table_info,
            args_parsed,
            table_args,
        }))
    }

    pub fn schema() -> Arc<TableSchema> {
        TableSchemaRefExt::create(vec![
            TableField::new("column_name", TableDataType::String),
            TableField::new("type", TableDataType::String),
            TableField::new("nullable", TableDataType::Boolean),
            TableField::new("order_id", TableDataType::Number(NumberDataType::UInt64)),
        ])
    }
}

#[async_trait::async_trait]
impl Table for InferSchemaTable {
    fn as_any(&self) -> &dyn Any {
        self
    }

    fn get_table_info(&self) -> &TableInfo {
        &self.table_info
    }

    #[async_backtrace::framed]
    async fn read_partitions(
        &self,
        _ctx: Arc<dyn TableContext>,
        _push_downs: Option<PushDownInfo>,
        _dry_run: bool,
    ) -> Result<(PartStatistics, Partitions)> {
        Ok((PartStatistics::default(), Partitions::default()))
    }

    fn table_args(&self) -> Option<TableArgs> {
        Some(self.table_args.clone())
    }

    fn read_data(
        &self,
        ctx: Arc<dyn TableContext>,
        _plan: &DataSourcePlan,
        pipeline: &mut Pipeline,
        _put_cache: bool,
    ) -> Result<()> {
        pipeline.add_source(
            |output| InferSchemaSource::create(ctx.clone(), output, self.args_parsed.clone()),
            1,
        )?;
        Ok(())
    }
}

impl TableFunction for InferSchemaTable {
    fn function_name(&self) -> &str {
        self.name()
    }

    fn as_table<'a>(self: Arc<Self>) -> Arc<dyn Table + 'a>
    where Self: 'a {
        self
    }
}

struct InferSchemaSource {
    is_finished: bool,
    ctx: Arc<dyn TableContext>,
    args_parsed: InferSchemaArgsParsed,
}

impl InferSchemaSource {
    pub fn create(
        ctx: Arc<dyn TableContext>,
        output: Arc<OutputPort>,
        args_parsed: InferSchemaArgsParsed,
    ) -> Result<ProcessorPtr> {
        AsyncSourcer::create(ctx.clone(), output, InferSchemaSource {
            is_finished: false,
            ctx,
            args_parsed,
        })
    }
}

#[async_trait::async_trait]
impl AsyncSource for InferSchemaSource {
    const NAME: &'static str = INFER_SCHEMA;

    #[async_trait::unboxed_simple]
    #[async_backtrace::framed]
    async fn generate(&mut self) -> Result<Option<DataBlock>> {
        if self.is_finished {
            return Ok(None);
        }
        self.is_finished = true;

        let file_location = if let Some(location) =
            self.args_parsed.location.clone().strip_prefix('@')
        {
            FileLocation::Stage(location.to_string())
        } else if let Some(connection_name) = &self.args_parsed.connection_name {
            let conn = self.ctx.get_connection(connection_name).await?;
            let uri = UriLocation::from_uri(
                self.args_parsed.location.clone(),
                "".to_string(),
                conn.storage_params,
            )?;
            let proto = conn.storage_type.parse::<Scheme>()?;
            if proto != uri.protocol.parse::<Scheme>()? {
                return Err(ErrorCode::BadArguments(format!(
                    "protocol from connection_name={connection_name} ({proto}) not match with uri protocol ({0}).",
                    uri.protocol
                )));
            }
            FileLocation::Uri(uri)
        } else {
            let uri = UriLocation::from_uri(
                self.args_parsed.location.clone(),
                "".to_string(),
                BTreeMap::default(),
            )?;
            FileLocation::Uri(uri)
        };
        let (stage_info, path) = resolve_file_location(self.ctx.as_ref(), &file_location).await?;
        let enable_experimental_rbac_check = self
            .ctx
            .get_settings()
            .get_enable_experimental_rbac_check()?;
        if enable_experimental_rbac_check {
            let visibility_checker = self.ctx.get_visibility_checker().await?;
            if !stage_info.is_temporary
                && !visibility_checker.check_stage_read_visibility(&stage_info.stage_name)
            {
                return Err(ErrorCode::PermissionDenied(format!(
                    "Permission denied, privilege READ is required on stage {} for user {}",
                    stage_info.stage_name.clone(),
                    &self.ctx.get_current_user()?.identity(),
                )));
            }
        }
        let files_info = StageFilesInfo {
            path: path.clone(),
            ..self.args_parsed.files_info.clone()
        };
        let operator = init_stage_operator(&stage_info)?;

        let first_file = files_info.first_file(&operator).await?;
        let file_format_params = match &self.args_parsed.file_format {
            Some(f) => self.ctx.get_file_format(f).await?,
            None => stage_info.file_format_params.clone(),
        };
        let use_parquet2 = self.ctx.get_settings().get_use_parquet2()?;
        let schema = match file_format_params.get_type() {
            StageFileFormatType::Parquet => {
                if use_parquet2 {
                    let arrow_schema =
                        read_parquet_schema_async(&operator, &first_file.path).await?;
                    TableSchema::try_from(&arrow_schema)?
                } else {
                    let arrow_schema = read_parquet_schema_async_rs(
                        &operator,
                        &first_file.path,
                        Some(first_file.size),
                    )
                    .await?;
                    TableSchema::try_from(&arrow_schema)?
                }
            }
            _ => {
                return Err(ErrorCode::BadArguments(
                    "infer_schema is currently limited to format Parquet",
                ));
            }
        };

        let mut names: Vec<String> = vec![];
        let mut types: Vec<String> = vec![];
        let mut nulls: Vec<bool> = vec![];

        for field in schema.fields().iter() {
            names.push(field.name().to_string());

            let non_null_type = field.data_type().remove_recursive_nullable();
            types.push(non_null_type.sql_name());
            nulls.push(field.is_nullable());
        }

        let order_ids = (0..schema.fields().len() as u64).collect::<Vec<_>>();

        let block = DataBlock::new_from_columns(vec![
            StringType::from_data(names),
            StringType::from_data(types),
            BooleanType::from_data(nulls),
            UInt64Type::from_data(order_ids),
        ]);
        Ok(Some(block))
    }
}
