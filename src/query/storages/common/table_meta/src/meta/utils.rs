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

use std::ops::Add;
use std::path::Path;

use chrono::DateTime;
use chrono::Datelike;
use chrono::TimeZone;
use chrono::Timelike;
use chrono::Utc;
use databend_common_exception::ErrorCode;
use databend_common_exception::Result;

pub fn trim_timestamp_to_micro_second(ts: DateTime<Utc>) -> DateTime<Utc> {
    Utc.with_ymd_and_hms(
        ts.year(),
        ts.month(),
        ts.day(),
        ts.hour(),
        ts.minute(),
        ts.second(),
    )
    .unwrap()
    .with_nanosecond(ts.timestamp_subsec_micros() * 1_000)
    .unwrap()
}

pub fn monotonically_increased_timestamp(
    timestamp: DateTime<Utc>,
    previous_timestamp: &Option<DateTime<Utc>>,
) -> DateTime<Utc> {
    if let Some(prev_instant) = previous_timestamp {
        // timestamp of the snapshot should always larger than the previous one's
        if prev_instant > &timestamp {
            // if local time is smaller, use the timestamp of previous snapshot, plus 1 ms
            return prev_instant.add(chrono::Duration::milliseconds(1));
        }
    }
    timestamp
}

pub fn is_possible_non_standard_decimal_block(block_full_path: &str) -> Result<bool> {
    let file_name = Path::new(block_full_path)
        .file_name()
        .ok_or_else(|| {
            ErrorCode::StorageOther(format!(
                "Illegal block path, no file name found: {}",
                block_full_path
            ))
        })?
        .to_str()
        .expect("File stem of a block full path should always be valid UTF-8");
    Ok(file_name < "g")
}
