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
use std::collections::HashMap;
use std::sync::Arc;

use common_catalog::table_context::TableContext;
use common_exception::Result;
use common_expression::types::BooleanType;
use common_expression::BlockMetaInfo;
use common_expression::BlockMetaInfoDowncast;
use common_expression::DataBlock;
use common_expression::DataSchemaRef;
use common_expression::FieldIndex;
use common_expression::Value;
use common_functions::BUILTIN_FUNCTIONS;
use common_pipeline_core::pipe::PipeItem;
use common_pipeline_core::processors::port::InputPort;
use common_pipeline_core::processors::port::OutputPort;
use common_pipeline_core::processors::processor::Event;
use common_pipeline_core::processors::processor::ProcessorPtr;
use common_pipeline_core::processors::Processor;
use common_sql::evaluator::BlockOperator;
use common_sql::executor::MatchExpr;
use common_storage::metrics::merge_into::metrics_inc_merge_into_append_blocks_counter;

use crate::operations::merge_into::mutator::DeleteByExprMutator;
use crate::operations::merge_into::mutator::UpdateByExprMutator;

enum MutationKind {
    Update(UpdateDataBlockMutation),
    Delete(DeleteDataBlockMutation),
}

#[derive(serde::Serialize, serde::Deserialize, Clone, Debug, PartialEq, Eq)]
pub enum RowIdKind {
    Update,
    Delete,
}

#[typetag::serde(name = "row_id_kind")]
impl BlockMetaInfo for RowIdKind {
    fn equals(&self, info: &Box<dyn BlockMetaInfo>) -> bool {
        RowIdKind::downcast_ref_from(info).is_some_and(|other| self == other)
    }

    fn clone_self(&self) -> Box<dyn BlockMetaInfo> {
        Box::new(self.clone())
    }
}

struct UpdateDataBlockMutation {
    update_mutator: UpdateByExprMutator,
}

struct DeleteDataBlockMutation {
    delete_mutator: DeleteByExprMutator,
}

pub struct MatchedSplitProcessor {
    input_port: Arc<InputPort>,
    output_port_row_id: Arc<OutputPort>,
    output_port_updated: Arc<OutputPort>,
    ops: Vec<MutationKind>,
    ctx: Arc<dyn TableContext>,
    update_projections: Vec<usize>,
    row_id_idx: usize,
    input_data: Option<DataBlock>,
    output_data_row_id_data: Vec<DataBlock>,
    output_data_updated_data: Option<DataBlock>,
    target_table_schema: DataSchemaRef,
}

impl MatchedSplitProcessor {
    pub fn create(
        ctx: Arc<dyn TableContext>,
        row_id_idx: usize,
        matched: MatchExpr,
        field_index_of_input_schema: HashMap<FieldIndex, usize>,
        input_schema: DataSchemaRef,
        target_table_schema: DataSchemaRef,
    ) -> Result<Self> {
        let mut ops = Vec::<MutationKind>::new();
        for item in matched.iter() {
            // delete
            if item.1.is_none() {
                let filter = item.0.as_ref().map(|expr| expr.as_expr(&BUILTIN_FUNCTIONS));
                ops.push(MutationKind::Delete(DeleteDataBlockMutation {
                    delete_mutator: DeleteByExprMutator::create(
                        filter.clone(),
                        ctx.get_function_context()?,
                        row_id_idx,
                    ),
                }))
            } else {
                let update_lists = item.1.as_ref().unwrap();
                let filter = item
                    .0
                    .as_ref()
                    .map(|condition| condition.as_expr(&BUILTIN_FUNCTIONS));

                ops.push(MutationKind::Update(UpdateDataBlockMutation {
                    update_mutator: UpdateByExprMutator::create(
                        filter,
                        ctx.get_function_context()?,
                        field_index_of_input_schema.clone(),
                        update_lists.clone(),
                        input_schema.num_fields(),
                    ),
                }))
            }
        }
        let mut update_projections = Vec::with_capacity(field_index_of_input_schema.len());
        for field_index in 0..field_index_of_input_schema.len() {
            update_projections.push(*field_index_of_input_schema.get(&field_index).unwrap());
        }
        let input_port = InputPort::create();
        let output_port_row_id = OutputPort::create();
        let output_port_updated = OutputPort::create();
        Ok(Self {
            ctx,
            input_port,
            output_data_row_id_data: Vec::new(),
            output_data_updated_data: None,
            input_data: None,
            output_port_row_id,
            output_port_updated,
            ops,
            row_id_idx,
            update_projections,
            target_table_schema,
        })
    }

    pub fn into_pipe_item(self) -> PipeItem {
        let input = self.input_port.clone();
        let output_port_row_id = self.output_port_row_id.clone();
        let output_port_updated = self.output_port_updated.clone();
        let processor_ptr = ProcessorPtr::create(Box::new(self));
        PipeItem::create(processor_ptr, vec![input], vec![
            output_port_row_id,
            output_port_updated,
        ])
    }
}

impl Processor for MatchedSplitProcessor {
    fn name(&self) -> String {
        "MatchedSplit".to_owned()
    }

    #[doc = " Reference used for downcast."]
    fn as_any(&mut self) -> &mut dyn Any {
        self
    }

    fn event(&mut self) -> Result<Event> {
        // 1. if there is no data and input_port is finished, this processor has finished
        // it's work
        let finished = self.input_port.is_finished()
            && self.output_data_row_id_data.is_empty()
            && self.output_data_updated_data.is_none();
        if finished {
            self.output_port_row_id.finish();
            self.output_port_updated.finish();
            return Ok(Event::Finished);
        }

        let mut pushed_something = false;

        // 2. process data stage here
        if self.output_port_row_id.can_push() && !self.output_data_row_id_data.is_empty() {
            self.output_port_row_id
                .push_data(Ok(self.output_data_row_id_data.pop().unwrap()));
            pushed_something = true
        }

        if self.output_port_updated.can_push() {
            if let Some(update_data) = self.output_data_updated_data.take() {
                self.output_port_updated.push_data(Ok(update_data));
                pushed_something = true
            }
        }

        // 3. trigger down stream pipeItem to consume if we pushed data
        if pushed_something {
            Ok(Event::NeedConsume)
        } else {
            // 4. we can't pushed data ,so the down stream is not prepared or we have no data at all
            // we need to make sure only when the all out_pudt_data are empty ,and we start to split
            // datablock held by input_data
            if self.input_port.has_data() {
                if self.output_data_row_id_data.is_empty()
                    && self.output_data_updated_data.is_none()
                {
                    // no pending data (being sent to down streams)
                    self.input_data = Some(self.input_port.pull_data().unwrap()?);
                    Ok(Event::Sync)
                } else {
                    // data pending
                    Ok(Event::NeedConsume)
                }
            } else {
                self.input_port.set_need_data();
                Ok(Event::NeedData)
            }
        }
    }

    // Todo:(JackTan25) accutally, we should do insert-only optimization in the future.
    fn process(&mut self) -> Result<()> {
        if let Some(data_block) = self.input_data.take() {
            if data_block.is_empty() {
                return Ok(());
            }
            // insert-only, we need to remove this pipeline according to strategy.
            if self.ops.is_empty() {
                return Ok(());
            }
            let mut current_block = data_block;
            for op in self.ops.iter() {
                match op {
                    MutationKind::Update(update_mutation) => {
                        let stage_block = update_mutation
                            .update_mutator
                            .update_by_expr(current_block)?;
                        current_block = stage_block;
                    }

                    MutationKind::Delete(delete_mutation) => {
                        let (stage_block, mut row_ids) = delete_mutation
                            .delete_mutator
                            .delete_by_expr(current_block)?;

                        if stage_block.is_empty() {
                            // delete all
                            if !row_ids.is_empty() {
                                row_ids = row_ids.add_meta(Some(Box::new(RowIdKind::Delete)))?;
                                self.output_data_row_id_data.push(row_ids);
                            }
                            return Ok(());
                        }
                        current_block = stage_block;
                    }
                }
            }

            let filter: Value<BooleanType> = current_block
                .get_by_offset(current_block.num_columns() - 1)
                .value
                .try_downcast()
                .unwrap();
            current_block = current_block.filter_boolean_value(&filter)?;
            if !current_block.is_empty() {
                // add updated row_ids
                self.output_data_row_id_data.push(DataBlock::new_with_meta(
                    vec![current_block.get_by_offset(self.row_id_idx).clone()],
                    current_block.num_rows(),
                    Some(Box::new(RowIdKind::Update)),
                ));
                let op = BlockOperator::Project {
                    projection: self.update_projections.clone(),
                };
                current_block = op.execute(&self.ctx.get_function_context()?, current_block)?;
                metrics_inc_merge_into_append_blocks_counter(1);
                current_block =
                    current_block.add_meta(Some(Box::new(self.target_table_schema.clone())))?;
                self.output_data_updated_data = Some(current_block);
            }
        }
        Ok(())
    }
}