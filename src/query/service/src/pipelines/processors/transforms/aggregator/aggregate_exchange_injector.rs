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

use std::marker::PhantomData;
use std::sync::Arc;

use bumpalo::Bump;
use databend_common_catalog::table_context::TableContext;
use databend_common_exception::ErrorCode;
use databend_common_exception::Result;
use databend_common_expression::BlockMetaInfoDowncast;
use databend_common_expression::DataBlock;
use databend_common_hashtable::FastHash;
use databend_common_hashtable::HashtableEntryMutRefLike;
use databend_common_hashtable::HashtableEntryRefLike;
use databend_common_hashtable::HashtableLike;
use databend_common_pipeline_core::processors::ProcessorPtr;
use databend_common_pipeline_core::query_spill_prefix;
use databend_common_pipeline_core::Pipeline;
use databend_common_settings::FlightCompression;
use databend_common_storage::DataOperator;
use strength_reduce::StrengthReducedU64;

use crate::api::DataExchange;
use crate::api::ExchangeInjector;
use crate::api::ExchangeSorting;
use crate::api::FlightScatter;
use crate::api::MergeExchangeParams;
use crate::api::ShuffleExchangeParams;
use crate::pipelines::processors::transforms::aggregator::aggregate_meta::AggregateMeta;
use crate::pipelines::processors::transforms::aggregator::aggregate_meta::HashTablePayload;
use crate::pipelines::processors::transforms::aggregator::serde::TransformExchangeAggregateSerializer;
use crate::pipelines::processors::transforms::aggregator::serde::TransformExchangeAsyncBarrier;
use crate::pipelines::processors::transforms::aggregator::serde::TransformExchangeGroupBySerializer;
use crate::pipelines::processors::transforms::aggregator::AggregatorParams;
use crate::pipelines::processors::transforms::aggregator::HashTableCell;
use crate::pipelines::processors::transforms::aggregator::TransformAggregateDeserializer;
use crate::pipelines::processors::transforms::aggregator::TransformAggregateSerializer;
use crate::pipelines::processors::transforms::aggregator::TransformAggregateSpillWriter;
use crate::pipelines::processors::transforms::aggregator::TransformGroupByDeserializer;
use crate::pipelines::processors::transforms::aggregator::TransformGroupBySerializer;
use crate::pipelines::processors::transforms::aggregator::TransformGroupBySpillWriter;
use crate::pipelines::processors::transforms::group_by::Area;
use crate::pipelines::processors::transforms::group_by::ArenaHolder;
use crate::pipelines::processors::transforms::group_by::HashMethodBounds;
use crate::pipelines::processors::transforms::group_by::PartitionedHashMethod;
use crate::sessions::QueryContext;

struct AggregateExchangeSorting<Method: HashMethodBounds, V: Send + Sync + 'static> {
    _phantom: PhantomData<(Method, V)>,
}

impl<Method: HashMethodBounds, V: Send + Sync + 'static> ExchangeSorting
    for AggregateExchangeSorting<Method, V>
{
    fn block_number(&self, data_block: &DataBlock) -> Result<isize> {
        match data_block.get_meta() {
            None => Ok(-1),
            Some(block_meta_info) => {
                match AggregateMeta::<Method, V>::downcast_ref_from(block_meta_info) {
                    None => Err(ErrorCode::Internal(format!(
                        "Internal error, AggregateExchangeSorting only recv AggregateMeta {:?}",
                        serde_json::to_string(block_meta_info)
                    ))),
                    Some(meta_info) => match meta_info {
                        AggregateMeta::Partitioned { .. } => unreachable!(),
                        AggregateMeta::Serialized(v) => Ok(v.bucket),
                        AggregateMeta::HashTable(v) => Ok(v.bucket),
                        AggregateMeta::AggregateHashTable(_) => unreachable!(),
                        AggregateMeta::Spilled(_)
                        | AggregateMeta::Spilling(_)
                        | AggregateMeta::BucketSpilled(_) => Ok(-1),
                    },
                }
            }
        }
    }
}

struct HashTableHashScatter<Method: HashMethodBounds, V: Copy + Send + Sync + 'static> {
    method: Method,
    buckets: usize,
    _phantom: PhantomData<V>,
}

fn scatter<Method: HashMethodBounds, V: Copy + Send + Sync + 'static>(
    mut payload: HashTablePayload<Method, V>,
    buckets: usize,
    method: &Method,
) -> Result<Vec<HashTableCell<Method, V>>> {
    let mut buckets = Vec::with_capacity(buckets);

    for _ in 0..buckets.capacity() {
        buckets.push(method.create_hash_table(Arc::new(Bump::new()))?);
    }

    let mods = StrengthReducedU64::new(buckets.len() as u64);
    for item in payload.cell.hashtable.iter() {
        let bucket_index = (item.key().fast_hash() % mods) as usize;

        unsafe {
            match buckets[bucket_index].insert_and_entry(item.key()) {
                Ok(mut entry) => {
                    *entry.get_mut() = *item.get();
                }
                Err(mut entry) => {
                    *entry.get_mut() = *item.get();
                }
            }
        }
    }

    let mut res = Vec::with_capacity(buckets.len());
    let dropper = payload.cell._dropper.take();
    let arena = std::mem::replace(&mut payload.cell.arena, Area::create());
    payload
        .cell
        .arena_holders
        .push(ArenaHolder::create(Some(arena)));

    for bucket_table in buckets {
        let mut cell = HashTableCell::<Method, V>::create(bucket_table, dropper.clone().unwrap());
        cell.arena_holders
            .extend(payload.cell.arena_holders.clone());
        res.push(cell);
    }

    Ok(res)
}

impl<Method: HashMethodBounds, V: Copy + Send + Sync + 'static> FlightScatter
    for HashTableHashScatter<Method, V>
{
    fn execute(&self, mut data_block: DataBlock) -> Result<Vec<DataBlock>> {
        if let Some(block_meta) = data_block.take_meta() {
            if let Some(block_meta) = AggregateMeta::<Method, V>::downcast_from(block_meta) {
                let mut blocks = Vec::with_capacity(self.buckets);
                match block_meta {
                    AggregateMeta::Spilled(_) => unreachable!(),
                    AggregateMeta::BucketSpilled(_) => unreachable!(),
                    AggregateMeta::Serialized(_) => unreachable!(),
                    AggregateMeta::Partitioned { .. } => unreachable!(),
                    AggregateMeta::Spilling(payload) => {
                        let method = PartitionedHashMethod::create(self.method.clone());
                        for hashtable_cell in scatter(payload, self.buckets, &method)? {
                            blocks.push(match hashtable_cell.hashtable.len() == 0 {
                                true => DataBlock::empty(),
                                false => DataBlock::empty_with_meta(
                                    AggregateMeta::<Method, V>::create_spilling(hashtable_cell),
                                ),
                            });
                        }
                    }
                    AggregateMeta::HashTable(payload) => {
                        let bucket = payload.bucket;
                        for hashtable_cell in scatter(payload, self.buckets, &self.method)? {
                            blocks.push(match hashtable_cell.hashtable.len() == 0 {
                                true => DataBlock::empty(),
                                false => DataBlock::empty_with_meta(
                                    AggregateMeta::<Method, V>::create_hashtable(
                                        bucket,
                                        hashtable_cell,
                                    ),
                                ),
                            });
                        }
                    }

                    AggregateMeta::AggregateHashTable(_) => todo!("AGG_HASHTABLE"),
                };

                return Ok(blocks);
            }
        }

        Err(ErrorCode::Internal(
            "Internal, HashTableHashScatter only recv AggregateMeta",
        ))
    }
}

pub struct AggregateInjector<Method: HashMethodBounds, V: Copy + Send + Sync + 'static> {
    ctx: Arc<QueryContext>,
    method: Method,
    tenant: String,
    aggregator_params: Arc<AggregatorParams>,
    _phantom: PhantomData<V>,
}

impl<Method: HashMethodBounds, V: Copy + Send + Sync + 'static> AggregateInjector<Method, V> {
    pub fn create(
        ctx: Arc<QueryContext>,
        method: Method,
        params: Arc<AggregatorParams>,
    ) -> Arc<dyn ExchangeInjector> {
        let tenant = ctx.get_tenant();
        Arc::new(AggregateInjector::<Method, V> {
            ctx,
            method,
            tenant,
            aggregator_params: params,
            _phantom: Default::default(),
        })
    }
}

impl<Method: HashMethodBounds, V: Copy + Send + Sync + 'static> ExchangeInjector
    for AggregateInjector<Method, V>
{
    fn flight_scatter(
        &self,
        _: &Arc<QueryContext>,
        exchange: &DataExchange,
    ) -> Result<Arc<Box<dyn FlightScatter>>> {
        match exchange {
            DataExchange::Merge(_) => unreachable!(),
            DataExchange::Broadcast(_) => unreachable!(),
            DataExchange::ShuffleDataExchange(exchange) => {
                Ok(Arc::new(Box::new(HashTableHashScatter::<Method, V> {
                    method: self.method.clone(),
                    buckets: exchange.destination_ids.len(),
                    _phantom: Default::default(),
                })))
            }
        }
    }

    fn exchange_sorting(&self) -> Option<Arc<dyn ExchangeSorting>> {
        Some(Arc::new(AggregateExchangeSorting::<Method, V> {
            _phantom: Default::default(),
        }))
    }

    fn apply_merge_serializer(
        &self,
        _: &MergeExchangeParams,
        _compression: Option<FlightCompression>,
        pipeline: &mut Pipeline,
    ) -> Result<()> {
        let method = &self.method;
        let params = self.aggregator_params.clone();

        let operator = DataOperator::instance().operator();
        let location_prefix = query_spill_prefix(&self.tenant);

        pipeline.add_transform(|input, output| {
            Ok(ProcessorPtr::create(
                match params.aggregate_functions.is_empty() {
                    true => TransformGroupBySpillWriter::create(
                        self.ctx.clone(),
                        input,
                        output,
                        method.clone(),
                        operator.clone(),
                        location_prefix.clone(),
                    ),
                    false => TransformAggregateSpillWriter::create(
                        self.ctx.clone(),
                        input,
                        output,
                        method.clone(),
                        operator.clone(),
                        params.clone(),
                        location_prefix.clone(),
                    ),
                },
            ))
        })?;

        pipeline.add_transform(
            |input, output| match params.aggregate_functions.is_empty() {
                true => TransformGroupBySerializer::try_create(input, output, method.clone()),
                false => TransformAggregateSerializer::try_create(
                    input,
                    output,
                    method.clone(),
                    params.clone(),
                ),
            },
        )
    }

    fn apply_shuffle_serializer(
        &self,
        shuffle_params: &ShuffleExchangeParams,
        compression: Option<FlightCompression>,
        pipeline: &mut Pipeline,
    ) -> Result<()> {
        let method = &self.method;
        let params = self.aggregator_params.clone();
        let operator = DataOperator::instance().operator();
        let location_prefix = query_spill_prefix(&self.tenant);

        let schema = shuffle_params.schema.clone();
        let local_id = &shuffle_params.executor_id;
        let local_pos = shuffle_params
            .destination_ids
            .iter()
            .position(|x| x == local_id)
            .unwrap();

        pipeline.add_transform(|input, output| {
            Ok(ProcessorPtr::create(
                match params.aggregate_functions.is_empty() {
                    true => TransformExchangeGroupBySerializer::create(
                        self.ctx.clone(),
                        input,
                        output,
                        method.clone(),
                        operator.clone(),
                        location_prefix.clone(),
                        schema.clone(),
                        local_pos,
                        compression,
                    ),
                    false => TransformExchangeAggregateSerializer::create(
                        self.ctx.clone(),
                        input,
                        output,
                        method.clone(),
                        operator.clone(),
                        location_prefix.clone(),
                        params.clone(),
                        compression,
                        schema.clone(),
                        local_pos,
                    ),
                },
            ))
        })?;

        pipeline.add_transform(TransformExchangeAsyncBarrier::try_create)
    }

    fn apply_merge_deserializer(
        &self,
        params: &MergeExchangeParams,
        pipeline: &mut Pipeline,
    ) -> Result<()> {
        pipeline.add_transform(|input, output| {
            match self.aggregator_params.aggregate_functions.is_empty() {
                true => TransformGroupByDeserializer::<Method>::try_create(
                    input,
                    output,
                    &params.schema,
                ),
                false => TransformAggregateDeserializer::<Method>::try_create(
                    input,
                    output,
                    &params.schema,
                ),
            }
        })
    }

    fn apply_shuffle_deserializer(
        &self,
        params: &ShuffleExchangeParams,
        pipeline: &mut Pipeline,
    ) -> Result<()> {
        pipeline.add_transform(|input, output| {
            match self.aggregator_params.aggregate_functions.is_empty() {
                true => TransformGroupByDeserializer::<Method>::try_create(
                    input,
                    output,
                    &params.schema,
                ),
                false => TransformAggregateDeserializer::<Method>::try_create(
                    input,
                    output,
                    &params.schema,
                ),
            }
        })
    }
}
