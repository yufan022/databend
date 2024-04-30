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
use std::collections::VecDeque;
use std::sync::Arc;

use databend_common_exception::ErrorCode;
use databend_common_exception::Result;
use databend_common_expression::types::binary::BinaryColumn;
use databend_common_expression::types::DataType;
use databend_common_expression::types::DateType;
use databend_common_expression::types::NumberDataType;
use databend_common_expression::types::NumberType;
use databend_common_expression::types::StringType;
use databend_common_expression::types::TimestampType;
use databend_common_expression::with_number_mapped_type;
use databend_common_expression::Column;
use databend_common_expression::DataBlock;
use databend_common_expression::DataSchemaRef;
use databend_common_expression::SortColumnDescription;
use databend_common_pipeline_core::processors::Event;
use databend_common_pipeline_core::processors::InputPort;
use databend_common_pipeline_core::processors::OutputPort;
use databend_common_pipeline_core::processors::Processor;
use databend_common_pipeline_core::processors::ProcessorPtr;
use databend_common_pipeline_core::Pipe;
use databend_common_pipeline_core::PipeItem;
use databend_common_pipeline_core::Pipeline;

use super::sort::HeapMerger;
use super::sort::Rows;
use super::sort::SimpleRows;
use super::sort::SortedStream;
use crate::processors::sort::utils::ORDER_COL_NAME;

pub fn try_add_multi_sort_merge(
    pipeline: &mut Pipeline,
    schema: DataSchemaRef,
    block_size: usize,
    limit: Option<usize>,
    sort_columns_descriptions: Arc<Vec<SortColumnDescription>>,
    remove_order_col: bool,
) -> Result<()> {
    debug_assert!(if !remove_order_col {
        schema.has_field(ORDER_COL_NAME)
    } else {
        !schema.has_field(ORDER_COL_NAME)
    });

    if pipeline.is_empty() {
        return Err(ErrorCode::Internal("Cannot resize empty pipe."));
    }

    match pipeline.output_len() {
        0 => Err(ErrorCode::Internal("Cannot resize empty pipe.")),
        1 => Ok(()),
        last_pipe_size => {
            let mut inputs_port = Vec::with_capacity(last_pipe_size);
            for _ in 0..last_pipe_size {
                inputs_port.push(InputPort::create());
            }
            let output_port = OutputPort::create();

            let processor = ProcessorPtr::create(create_processor(
                inputs_port.clone(),
                output_port.clone(),
                schema,
                block_size,
                limit,
                sort_columns_descriptions,
                remove_order_col,
            )?);

            pipeline.add_pipe(Pipe::create(inputs_port.len(), 1, vec![PipeItem::create(
                processor,
                inputs_port,
                vec![output_port],
            )]));

            Ok(())
        }
    }
}

fn create_processor(
    inputs: Vec<Arc<InputPort>>,
    output: Arc<OutputPort>,
    schema: DataSchemaRef,
    block_size: usize,
    limit: Option<usize>,
    sort_columns_descriptions: Arc<Vec<SortColumnDescription>>,
    remove_order_col: bool,
) -> Result<Box<dyn Processor>> {
    Ok(if sort_columns_descriptions.len() == 1 {
        let sort_type = schema
            .field(sort_columns_descriptions[0].offset)
            .data_type();
        match sort_type {
            DataType::Number(num_ty) => with_number_mapped_type!(|NUM_TYPE| match num_ty {
                NumberDataType::NUM_TYPE => Box::new(MultiSortMergeProcessor::<
                    SimpleRows<NumberType<NUM_TYPE>>,
                >::create(
                    inputs,
                    output,
                    schema,
                    block_size,
                    limit,
                    sort_columns_descriptions,
                    remove_order_col,
                )?),
            }),
            DataType::Date => Box::new(MultiSortMergeProcessor::<SimpleRows<DateType>>::create(
                inputs,
                output,
                schema,
                block_size,
                limit,
                sort_columns_descriptions,
                remove_order_col,
            )?),
            DataType::Timestamp => Box::new(
                MultiSortMergeProcessor::<SimpleRows<TimestampType>>::create(
                    inputs,
                    output,
                    schema,
                    block_size,
                    limit,
                    sort_columns_descriptions,
                    remove_order_col,
                )?,
            ),
            DataType::String => {
                Box::new(MultiSortMergeProcessor::<SimpleRows<StringType>>::create(
                    inputs,
                    output,
                    schema,
                    block_size,
                    limit,
                    sort_columns_descriptions,
                    remove_order_col,
                )?)
            }
            _ => Box::new(MultiSortMergeProcessor::<BinaryColumn>::create(
                inputs,
                output,
                schema,
                block_size,
                limit,
                sort_columns_descriptions,
                remove_order_col,
            )?),
        }
    } else {
        Box::new(MultiSortMergeProcessor::<BinaryColumn>::create(
            inputs,
            output,
            schema,
            block_size,
            limit,
            sort_columns_descriptions,
            remove_order_col,
        )?)
    })
}

struct BlockStream {
    input: Arc<InputPort>,
    remove_order_col: bool,
}

impl BlockStream {
    fn new(input: Arc<InputPort>, remove_order_col: bool) -> Self {
        Self {
            input,
            remove_order_col,
        }
    }
}

impl SortedStream for BlockStream {
    fn next(&mut self) -> Result<(Option<(DataBlock, Column)>, bool)> {
        if self.input.has_data() {
            let mut block = self.input.pull_data().unwrap()?;
            let col = block.get_last_column().clone();
            if self.remove_order_col {
                block.pop_columns(1);
            }
            self.input.set_need_data();
            Ok((Some((block, col)), false))
        } else if self.input.is_finished() {
            Ok((None, false))
        } else {
            self.input.set_need_data();
            Ok((None, true))
        }
    }
}

/// TransformMultiSortMerge is a processor with multiple input ports;
pub struct MultiSortMergeProcessor<R>
where R: Rows
{
    merger: HeapMerger<R, BlockStream>,

    /// This field is used to drive the processor's state.
    ///
    /// There is a copy of this fields in `self.merger` and it will pull data from it.
    inputs: Vec<Arc<InputPort>>,
    output: Arc<OutputPort>,

    output_data: VecDeque<DataBlock>,
}

impl<R> MultiSortMergeProcessor<R>
where R: Rows
{
    pub fn create(
        inputs: Vec<Arc<InputPort>>,
        output: Arc<OutputPort>,
        schema: DataSchemaRef,
        block_size: usize,
        limit: Option<usize>,
        sort_desc: Arc<Vec<SortColumnDescription>>,
        remove_order_col: bool,
    ) -> Result<Self> {
        let streams = inputs
            .iter()
            .map(|i| BlockStream::new(i.clone(), remove_order_col))
            .collect::<Vec<_>>();
        let merger = HeapMerger::create(schema, streams, sort_desc, block_size, limit);
        Ok(Self {
            merger,
            inputs,
            output,
            output_data: VecDeque::new(),
        })
    }
}

impl<R> Processor for MultiSortMergeProcessor<R>
where R: Rows + Send + 'static
{
    fn name(&self) -> String {
        "MultiSortMerge".to_string()
    }

    fn as_any(&mut self) -> &mut dyn Any {
        self
    }

    fn event(&mut self) -> Result<Event> {
        if self.output.is_finished() {
            for input in self.inputs.iter() {
                input.finish();
            }
            return Ok(Event::Finished);
        }

        if !self.output.can_push() {
            return Ok(Event::NeedConsume);
        }

        if let Some(block) = self.output_data.pop_front() {
            self.output.push_data(Ok(block));
            return Ok(Event::NeedConsume);
        }

        if self.merger.is_finished() {
            self.output.finish();
            for input in self.inputs.iter() {
                input.finish();
            }
            return Ok(Event::Finished);
        }

        self.merger.poll_pending_stream()?;

        if self.merger.has_pending_stream() {
            Ok(Event::NeedData)
        } else {
            Ok(Event::Sync)
        }
    }

    fn process(&mut self) -> Result<()> {
        while let Some(block) = self.merger.next_block()? {
            self.output_data.push_back(block);
        }
        Ok(())
    }
}
