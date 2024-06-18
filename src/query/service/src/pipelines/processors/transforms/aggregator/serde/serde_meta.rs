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

use std::ops::Range;

use databend_common_expression::BlockMetaInfo;
use databend_common_expression::BlockMetaInfoDowncast;
use databend_common_expression::BlockMetaInfoPtr;

pub const BUCKET_TYPE: usize = 1;
pub const SPILLED_TYPE: usize = 2;

// Cannot change to enum, because bincode cannot deserialize custom enum
#[derive(serde::Serialize, serde::Deserialize, Clone, Debug, PartialEq)]
pub struct AggregateSerdeMeta {
    pub typ: usize,
    pub bucket: isize,
    pub location: Option<String>,
    pub data_range: Option<Range<u64>>,
    pub columns_layout: Vec<usize>,
    // use for new agg_hashtable
    pub is_agg_payload: bool,
    pub max_partition_count: usize,
}

impl AggregateSerdeMeta {
    pub fn create(bucket: isize) -> BlockMetaInfoPtr {
        Box::new(AggregateSerdeMeta {
            typ: BUCKET_TYPE,
            bucket,
            location: None,
            data_range: None,
            columns_layout: vec![],
            is_agg_payload: false,
            max_partition_count: 0,
        })
    }

    pub fn create_agg_payload(bucket: isize, max_partition_count: usize) -> BlockMetaInfoPtr {
        Box::new(AggregateSerdeMeta {
            typ: BUCKET_TYPE,
            bucket,
            location: None,
            data_range: None,
            columns_layout: vec![],
            is_agg_payload: true,
            max_partition_count,
        })
    }

    pub fn create_spilled(
        bucket: isize,
        location: String,
        data_range: Range<u64>,
        columns_layout: Vec<usize>,
    ) -> BlockMetaInfoPtr {
        Box::new(AggregateSerdeMeta {
            typ: SPILLED_TYPE,
            bucket,
            columns_layout,
            location: Some(location),
            data_range: Some(data_range),
            is_agg_payload: false,
            max_partition_count: 0,
        })
    }
}

#[typetag::serde(name = "aggregate_serde")]
impl BlockMetaInfo for AggregateSerdeMeta {
    fn equals(&self, info: &Box<dyn BlockMetaInfo>) -> bool {
        AggregateSerdeMeta::downcast_ref_from(info).is_some_and(|other| self == other)
    }

    fn clone_self(&self) -> Box<dyn BlockMetaInfo> {
        Box::new(self.clone())
    }
}
