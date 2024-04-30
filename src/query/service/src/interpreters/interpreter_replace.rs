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

use databend_common_catalog::table::TableExt;
use databend_common_catalog::table_context::TableContext;
use databend_common_exception::ErrorCode;
use databend_common_exception::Result;
use databend_common_expression::DataSchemaRef;
use databend_common_functions::BUILTIN_FUNCTIONS;
use databend_common_meta_app::principal::StageInfo;
use databend_common_meta_app::schema::UpdateStreamMetaReq;
use databend_common_sql::executor::cast_expr_to_non_null_boolean;
use databend_common_sql::executor::physical_plans::CommitSink;
use databend_common_sql::executor::physical_plans::Exchange;
use databend_common_sql::executor::physical_plans::FragmentKind;
use databend_common_sql::executor::physical_plans::MutationKind;
use databend_common_sql::executor::physical_plans::OnConflictField;
use databend_common_sql::executor::physical_plans::ReplaceAsyncSourcer;
use databend_common_sql::executor::physical_plans::ReplaceDeduplicate;
use databend_common_sql::executor::physical_plans::ReplaceInto;
use databend_common_sql::executor::physical_plans::ReplaceSelectCtx;
use databend_common_sql::executor::PhysicalPlan;
use databend_common_sql::plans::InsertInputSource;
use databend_common_sql::plans::Plan;
use databend_common_sql::plans::Replace;
use databend_common_sql::BindContext;
use databend_common_sql::Metadata;
use databend_common_sql::NameResolutionContext;
use databend_common_sql::ScalarBinder;
use databend_common_storage::StageFileInfo;
use databend_common_storages_factory::Table;
use databend_common_storages_fuse::FuseTable;
use databend_storages_common_table_meta::meta::TableSnapshot;
use parking_lot::RwLock;

use crate::interpreters::common::build_update_stream_meta_seq;
use crate::interpreters::common::check_deduplicate_label;
use crate::interpreters::interpreter_copy_into_table::CopyIntoTableInterpreter;
use crate::interpreters::HookOperator;
use crate::interpreters::Interpreter;
use crate::interpreters::InterpreterPtr;
use crate::interpreters::SelectInterpreter;
use crate::pipelines::PipelineBuildResult;
use crate::pipelines::PipelineBuilder;
use crate::schedulers::build_query_pipeline_without_render_result_set;
use crate::sessions::QueryContext;

pub struct ReplaceInterpreter {
    ctx: Arc<QueryContext>,
    plan: Replace,
}

impl ReplaceInterpreter {
    pub fn try_create(ctx: Arc<QueryContext>, plan: Replace) -> Result<InterpreterPtr> {
        Ok(Arc::new(ReplaceInterpreter { ctx, plan }))
    }
}

#[async_trait::async_trait]
impl Interpreter for ReplaceInterpreter {
    fn name(&self) -> &str {
        "ReplaceIntoInterpreter"
    }

    #[async_backtrace::framed]
    async fn execute2(&self) -> Result<PipelineBuildResult> {
        if check_deduplicate_label(self.ctx.clone()).await? {
            return Ok(PipelineBuildResult::create());
        }

        self.check_on_conflicts()?;

        // replace
        let (physical_plan, purge_info) = self.build_physical_plan().await?;
        let mut pipeline =
            build_query_pipeline_without_render_result_set(&self.ctx, &physical_plan).await?;

        // purge
        if let Some((files, stage_info)) = purge_info {
            PipelineBuilder::set_purge_files_on_finished(
                self.ctx.clone(),
                files,
                stage_info.copy_options.purge,
                stage_info,
                &mut pipeline.main_pipeline,
            )?;
        }

        // Execute hook.
        {
            let hook_operator = HookOperator::create(
                self.ctx.clone(),
                self.plan.catalog.clone(),
                self.plan.database.clone(),
                self.plan.table.clone(),
                "replace_into".to_owned(),
                true,
            );
            hook_operator.execute(&mut pipeline.main_pipeline).await;
        }

        Ok(pipeline)
    }
}

impl ReplaceInterpreter {
    async fn build_physical_plan(
        &self,
    ) -> Result<(Box<PhysicalPlan>, Option<(Vec<StageFileInfo>, StageInfo)>)> {
        let plan = &self.plan;
        let table = self
            .ctx
            .get_table(&plan.catalog, &plan.database, &plan.table)
            .await?;

        // check mutability
        table.check_mutable()?;
        // check change tracking
        if table.change_tracking_enabled() {
            return Err(ErrorCode::Unimplemented(format!(
                "change tracking is enabled for table '{}', does not support REPLACE",
                table.name(),
            )));
        }

        let catalog = self.ctx.get_catalog(&plan.catalog).await?;
        let schema = table.schema();
        let mut on_conflicts = Vec::with_capacity(plan.on_conflict_fields.len());
        for f in &plan.on_conflict_fields {
            let field_name = f.name();
            let (field_index, _) = match schema.column_with_name(field_name) {
                Some(idx) => idx,
                None => {
                    return Err(ErrorCode::Internal(
                        "not expected, on conflict field not found (after binding)",
                    ));
                }
            };
            on_conflicts.push(OnConflictField {
                table_field: f.clone(),
                field_index,
            })
        }
        let fuse_table = table.as_any().downcast_ref::<FuseTable>().ok_or_else(|| {
            ErrorCode::Unimplemented(format!(
                "table {}, engine type {}, does not support REPLACE INTO",
                table.name(),
                table.get_table_info().engine(),
            ))
        })?;

        let table_info = fuse_table.get_table_info();
        let base_snapshot = fuse_table.read_table_snapshot().await?.unwrap_or_else(|| {
            Arc::new(TableSnapshot::new_empty_snapshot(schema.as_ref().clone()))
        });

        let is_multi_node = !self.ctx.get_cluster().is_empty();
        let is_value_source = matches!(self.plan.source, InsertInputSource::Values { .. });
        let is_distributed = is_multi_node
            && !is_value_source
            && self.ctx.get_settings().get_enable_distributed_replace()?;
        let table_is_empty = base_snapshot.segments.is_empty();
        let table_level_range_index = base_snapshot.summary.col_stats.clone();
        let mut purge_info = None;

        let ReplaceSourceCtx {
            mut root,
            select_ctx,
            update_stream_meta,
            bind_context,
        } = self
            .connect_input_source(
                self.ctx.clone(),
                &self.plan.source,
                self.plan.schema(),
                &mut purge_info,
            )
            .await?;
        if let Some(s) = &select_ctx {
            let select_schema = s.select_schema.as_ref();
            // validate schema
            if select_schema.fields().len() < plan.schema().fields().len() {
                return Err(ErrorCode::BadArguments(
                    "Fields in select statement is less than expected",
                ));
            }
        }

        let delete_when = if let Some(expr) = &plan.delete_when {
            if bind_context.is_none() {
                return Err(ErrorCode::Unimplemented(
                    "Delete semantic is only supported in subquery",
                ));
            }
            let mut bind_context = bind_context.unwrap();
            let name_resolution_ctx =
                NameResolutionContext::try_from(self.ctx.get_settings().as_ref())?;
            let metadata = Arc::new(RwLock::new(Metadata::default()));
            let mut scalar_binder = ScalarBinder::new(
                &mut bind_context,
                self.ctx.clone(),
                &name_resolution_ctx,
                metadata,
                &[],
                Default::default(),
                Default::default(),
            );
            let (scalar, _) = scalar_binder.bind(expr).await?;
            let columns = scalar.used_columns();
            if columns.len() != 1 {
                return Err(ErrorCode::BadArguments(
                    "Delete must have one column in predicate",
                ));
            }
            let delete_column = columns.iter().next().unwrap();
            let column_bindings = &bind_context.columns;
            let delete_column_binding = column_bindings.iter().find(|c| c.index == *delete_column);
            if delete_column_binding.is_none() {
                return Err(ErrorCode::BadArguments(
                    "Delete must have one column in predicate",
                ));
            }
            let delete_column_name = delete_column_binding.unwrap().column_name.clone();
            let filter = cast_expr_to_non_null_boolean(
                scalar.as_expr()?.project_column_ref(|col| col.index),
            )?;

            let filter = filter.as_remote_expr();

            let expr = filter.as_expr(&BUILTIN_FUNCTIONS);
            if !expr.is_deterministic(&BUILTIN_FUNCTIONS) {
                return Err(ErrorCode::Unimplemented(
                    "Delete must have deterministic predicate",
                ));
            }
            Some((filter, delete_column_name))
        } else {
            None
        };

        // remove top exchange
        if let PhysicalPlan::Exchange(Exchange { input, .. }) = root.as_ref() {
            root = input.clone();
        }
        if is_distributed {
            root = Box::new(PhysicalPlan::Exchange(Exchange {
                plan_id: 0,
                input: root,
                kind: FragmentKind::Expansive,
                keys: vec![],
                allow_adjust_parallelism: true,
                ignore_exchange: false,
            }));
        }

        let max_num_pruning_columns = self
            .ctx
            .get_settings()
            .get_replace_into_bloom_pruning_max_column_number()?;
        let bloom_filter_column_indexes = if !table.cluster_keys(self.ctx.clone()).is_empty() {
            fuse_table
                .choose_bloom_filter_columns(
                    self.ctx.clone(),
                    &on_conflicts,
                    max_num_pruning_columns,
                )
                .await?
        } else {
            vec![]
        };

        root = Box::new(PhysicalPlan::ReplaceDeduplicate(Box::new(
            ReplaceDeduplicate {
                input: root,
                on_conflicts: on_conflicts.clone(),
                bloom_filter_column_indexes: bloom_filter_column_indexes.clone(),
                table_is_empty,
                table_info: table_info.clone(),
                catalog_info: catalog.info(),
                select_ctx,
                table_schema: plan.schema.clone(),
                table_level_range_index,
                need_insert: true,
                delete_when,
            },
        )));
        root = Box::new(PhysicalPlan::ReplaceInto(Box::new(ReplaceInto {
            input: root,
            block_thresholds: fuse_table.get_block_thresholds(),
            table_info: table_info.clone(),
            catalog_info: catalog.info(),
            on_conflicts,
            bloom_filter_column_indexes,
            segments: base_snapshot
                .segments
                .clone()
                .into_iter()
                .enumerate()
                .collect(),
            block_slots: None,
            need_insert: true,
        })));
        if is_distributed {
            root = Box::new(PhysicalPlan::Exchange(Exchange {
                plan_id: 0,
                input: root,
                kind: FragmentKind::Merge,
                keys: vec![],
                allow_adjust_parallelism: true,
                ignore_exchange: false,
            }));
        }
        root = Box::new(PhysicalPlan::CommitSink(Box::new(CommitSink {
            input: root,
            snapshot: base_snapshot,
            table_info: table_info.clone(),
            catalog_info: catalog.info(),
            mutation_kind: MutationKind::Replace,
            update_stream_meta: update_stream_meta.clone(),
            merge_meta: false,
            need_lock: false,
            deduplicated_label: unsafe { self.ctx.get_settings().get_deduplicate_label()? },
        })));
        Ok((root, purge_info))
    }

    fn check_on_conflicts(&self) -> Result<()> {
        if self.plan.on_conflict_fields.is_empty() {
            Err(ErrorCode::BadArguments(
                "at least one column must be specified in the replace into .. on [conflict] statement",
            ))
        } else {
            Ok(())
        }
    }

    #[async_backtrace::framed]
    async fn connect_input_source<'a>(
        &'a self,
        ctx: Arc<QueryContext>,
        source: &'a InsertInputSource,
        schema: DataSchemaRef,
        purge_info: &mut Option<(Vec<StageFileInfo>, StageInfo)>,
    ) -> Result<ReplaceSourceCtx> {
        match source {
            InsertInputSource::Values { data, start } => self
                .connect_value_source(schema.clone(), data, *start)
                .map(|root| ReplaceSourceCtx {
                    root,
                    select_ctx: None,
                    update_stream_meta: vec![],
                    bind_context: None,
                }),

            InsertInputSource::SelectPlan(plan) => {
                self.connect_query_plan_source(ctx.clone(), plan).await
            }
            InsertInputSource::Stage(plan) => match *plan.clone() {
                Plan::CopyIntoTable(copy_plan) => {
                    let interpreter =
                        CopyIntoTableInterpreter::try_create(ctx.clone(), *copy_plan.clone())?;
                    let (physical_plan, files, _) =
                        interpreter.build_physical_plan(&copy_plan).await?;
                    *purge_info = Some((files, copy_plan.stage_table_info.stage_info.clone()));
                    Ok(ReplaceSourceCtx {
                        root: Box::new(physical_plan),
                        select_ctx: None,
                        update_stream_meta: vec![],
                        bind_context: None,
                    })
                }
                _ => unreachable!("plan in InsertInputSource::Stag must be CopyIntoTable"),
            },
            _ => Err(ErrorCode::Unimplemented(
                "input source other than literal VALUES and sub queries are NOT supported yet.",
            )),
        }
    }

    fn connect_value_source(
        &self,
        schema: DataSchemaRef,
        value_data: &str,
        span_offset: usize,
    ) -> Result<Box<PhysicalPlan>> {
        Ok(Box::new(PhysicalPlan::ReplaceAsyncSourcer(
            ReplaceAsyncSourcer {
                value_data: value_data.to_string(),
                start: span_offset,
                schema,
            },
        )))
    }

    #[async_backtrace::framed]
    async fn connect_query_plan_source<'a>(
        &'a self,
        ctx: Arc<QueryContext>,
        query_plan: &Plan,
    ) -> Result<ReplaceSourceCtx> {
        let (s_expr, metadata, bind_context, formatted_ast) = match query_plan {
            Plan::Query {
                s_expr,
                metadata,
                bind_context,
                formatted_ast,
                ..
            } => (s_expr, metadata, bind_context, formatted_ast),
            v => unreachable!("Input plan must be Query, but it's {}", v),
        };

        let update_stream_meta = build_update_stream_meta_seq(self.ctx.clone(), metadata).await?;

        let select_interpreter = SelectInterpreter::try_create(
            ctx.clone(),
            *(bind_context.clone()),
            *s_expr.clone(),
            metadata.clone(),
            formatted_ast.clone(),
            false,
        )?;

        let physical_plan = select_interpreter
            .build_physical_plan()
            .await
            .map(Box::new)?;
        let select_ctx = ReplaceSelectCtx {
            select_column_bindings: bind_context.columns.clone(),
            select_schema: query_plan.schema(),
        };
        Ok(ReplaceSourceCtx {
            root: physical_plan,
            select_ctx: Some(select_ctx),
            update_stream_meta,
            bind_context: Some(*bind_context.clone()),
        })
    }
}

struct ReplaceSourceCtx {
    root: Box<PhysicalPlan>,
    select_ctx: Option<ReplaceSelectCtx>,
    update_stream_meta: Vec<UpdateStreamMetaReq>,
    bind_context: Option<BindContext>,
}
