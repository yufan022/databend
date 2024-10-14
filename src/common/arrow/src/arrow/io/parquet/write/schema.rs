// Copyright 2020-2022 Jorge C. Leitão
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

use base64::engine::general_purpose;
use base64::Engine as _;
use parquet2::metadata::KeyValue;
use parquet2::schema::types::GroupConvertedType;
use parquet2::schema::types::GroupLogicalType;
use parquet2::schema::types::IntegerType;
use parquet2::schema::types::ParquetType;
use parquet2::schema::types::PhysicalType;
use parquet2::schema::types::PrimitiveConvertedType;
use parquet2::schema::types::PrimitiveLogicalType;
use parquet2::schema::types::TimeUnit as ParquetTimeUnit;
use parquet2::schema::Repetition;

use super::super::ARROW_SCHEMA_META_KEY;
use crate::arrow::datatypes::DataType;
use crate::arrow::datatypes::Field;
use crate::arrow::datatypes::Schema;
use crate::arrow::datatypes::TimeUnit;
use crate::arrow::error::Error;
use crate::arrow::error::Result;
use crate::arrow::io::ipc::write::default_ipc_fields;
use crate::arrow::io::ipc::write::schema_to_bytes;
use crate::arrow::io::parquet::write::decimal_length_from_precision;

pub fn schema_to_metadata_key(schema: &Schema) -> KeyValue {
    let serialized_schema = schema_to_bytes(schema, &default_ipc_fields(&schema.fields));

    // manually prepending the length to the schema as arrow uses the legacy IPC format
    // TODO: change after addressing ARROW-9777
    let schema_len = serialized_schema.len();
    let mut len_prefix_schema = Vec::with_capacity(schema_len + 8);
    len_prefix_schema.extend_from_slice(&[255u8, 255, 255, 255]);
    len_prefix_schema.extend_from_slice(&(schema_len as u32).to_le_bytes());
    len_prefix_schema.extend_from_slice(&serialized_schema);

    let encoded = general_purpose::STANDARD.encode(&len_prefix_schema);

    KeyValue {
        key: ARROW_SCHEMA_META_KEY.to_string(),
        value: Some(encoded),
    }
}

// For arrow2 parquet, decimal256 will use 32 width if precision > 38
pub fn to_parquet_type(field: &Field) -> Result<ParquetType> {
    to_parquet_type_with_options(field, true)
}

/// Creates a [`ParquetType`] from a [`Field`].
pub fn to_parquet_type_with_options(field: &Field, decimal256_max: bool) -> Result<ParquetType> {
    let name = field.name.clone();
    let repetition = if field.is_nullable {
        Repetition::Optional
    } else {
        Repetition::Required
    };
    // create type from field
    match field.data_type().to_logical_type() {
        DataType::Null => Ok(ParquetType::try_from_primitive(
            name,
            PhysicalType::Int32,
            repetition,
            None,
            Some(PrimitiveLogicalType::Unknown),
            None,
        )?),
        DataType::Boolean => Ok(ParquetType::try_from_primitive(
            name,
            PhysicalType::Boolean,
            repetition,
            None,
            None,
            None,
        )?),
        DataType::Int32 => Ok(ParquetType::try_from_primitive(
            name,
            PhysicalType::Int32,
            repetition,
            None,
            None,
            None,
        )?),
        // DataType::Duration(_) has no parquet representation => do not apply any logical type
        DataType::Int64 | DataType::Duration(_) => Ok(ParquetType::try_from_primitive(
            name,
            PhysicalType::Int64,
            repetition,
            None,
            None,
            None,
        )?),
        // no natural representation in parquet; leave it as is.
        // arrow consumers MAY use the arrow schema in the metadata to parse them.
        DataType::Date64 => Ok(ParquetType::try_from_primitive(
            name,
            PhysicalType::Int64,
            repetition,
            None,
            None,
            None,
        )?),
        DataType::Float32 => Ok(ParquetType::try_from_primitive(
            name,
            PhysicalType::Float,
            repetition,
            None,
            None,
            None,
        )?),
        DataType::Float64 => Ok(ParquetType::try_from_primitive(
            name,
            PhysicalType::Double,
            repetition,
            None,
            None,
            None,
        )?),
        DataType::Binary | DataType::LargeBinary => Ok(ParquetType::try_from_primitive(
            name,
            PhysicalType::ByteArray,
            repetition,
            None,
            None,
            None,
        )?),
        DataType::Utf8 | DataType::LargeUtf8 => Ok(ParquetType::try_from_primitive(
            name,
            PhysicalType::ByteArray,
            repetition,
            Some(PrimitiveConvertedType::Utf8),
            Some(PrimitiveLogicalType::String),
            None,
        )?),
        DataType::Date32 => Ok(ParquetType::try_from_primitive(
            name,
            PhysicalType::Int32,
            repetition,
            Some(PrimitiveConvertedType::Date),
            Some(PrimitiveLogicalType::Date),
            None,
        )?),
        DataType::Int8 => Ok(ParquetType::try_from_primitive(
            name,
            PhysicalType::Int32,
            repetition,
            Some(PrimitiveConvertedType::Int8),
            Some(PrimitiveLogicalType::Integer(IntegerType::Int8)),
            None,
        )?),
        DataType::Int16 => Ok(ParquetType::try_from_primitive(
            name,
            PhysicalType::Int32,
            repetition,
            Some(PrimitiveConvertedType::Int16),
            Some(PrimitiveLogicalType::Integer(IntegerType::Int16)),
            None,
        )?),
        DataType::UInt8 => Ok(ParquetType::try_from_primitive(
            name,
            PhysicalType::Int32,
            repetition,
            Some(PrimitiveConvertedType::Uint8),
            Some(PrimitiveLogicalType::Integer(IntegerType::UInt8)),
            None,
        )?),
        DataType::UInt16 => Ok(ParquetType::try_from_primitive(
            name,
            PhysicalType::Int32,
            repetition,
            Some(PrimitiveConvertedType::Uint16),
            Some(PrimitiveLogicalType::Integer(IntegerType::UInt16)),
            None,
        )?),
        DataType::UInt32 => Ok(ParquetType::try_from_primitive(
            name,
            PhysicalType::Int32,
            repetition,
            Some(PrimitiveConvertedType::Uint32),
            Some(PrimitiveLogicalType::Integer(IntegerType::UInt32)),
            None,
        )?),
        DataType::UInt64 => Ok(ParquetType::try_from_primitive(
            name,
            PhysicalType::Int64,
            repetition,
            Some(PrimitiveConvertedType::Uint64),
            Some(PrimitiveLogicalType::Integer(IntegerType::UInt64)),
            None,
        )?),
        // no natural representation in parquet; leave it as is.
        // arrow consumers MAY use the arrow schema in the metadata to parse them.
        DataType::Timestamp(TimeUnit::Second, _) => Ok(ParquetType::try_from_primitive(
            name,
            PhysicalType::Int64,
            repetition,
            None,
            None,
            None,
        )?),
        DataType::Timestamp(time_unit, zone) => Ok(ParquetType::try_from_primitive(
            name,
            PhysicalType::Int64,
            repetition,
            None,
            Some(PrimitiveLogicalType::Timestamp {
                is_adjusted_to_utc: matches!(zone, Some(z) if !z.as_str().is_empty()),
                unit: match time_unit {
                    TimeUnit::Second => unreachable!(),
                    TimeUnit::Millisecond => ParquetTimeUnit::Milliseconds,
                    TimeUnit::Microsecond => ParquetTimeUnit::Microseconds,
                    TimeUnit::Nanosecond => ParquetTimeUnit::Nanoseconds,
                },
            }),
            None,
        )?),
        // no natural representation in parquet; leave it as is.
        // arrow consumers MAY use the arrow schema in the metadata to parse them.
        DataType::Time32(TimeUnit::Second) => Ok(ParquetType::try_from_primitive(
            name,
            PhysicalType::Int32,
            repetition,
            None,
            None,
            None,
        )?),
        DataType::Time32(TimeUnit::Millisecond) => Ok(ParquetType::try_from_primitive(
            name,
            PhysicalType::Int32,
            repetition,
            Some(PrimitiveConvertedType::TimeMillis),
            Some(PrimitiveLogicalType::Time {
                is_adjusted_to_utc: false,
                unit: ParquetTimeUnit::Milliseconds,
            }),
            None,
        )?),
        DataType::Time64(time_unit) => Ok(ParquetType::try_from_primitive(
            name,
            PhysicalType::Int64,
            repetition,
            match time_unit {
                TimeUnit::Microsecond => Some(PrimitiveConvertedType::TimeMicros),
                TimeUnit::Nanosecond => None,
                _ => unreachable!(),
            },
            Some(PrimitiveLogicalType::Time {
                is_adjusted_to_utc: false,
                unit: match time_unit {
                    TimeUnit::Microsecond => ParquetTimeUnit::Microseconds,
                    TimeUnit::Nanosecond => ParquetTimeUnit::Nanoseconds,
                    _ => unreachable!(),
                },
            }),
            None,
        )?),
        DataType::Struct(fields) => {
            if fields.is_empty() {
                return Err(Error::InvalidArgumentError(
                    "Parquet does not support writing empty structs".to_string(),
                ));
            }
            // recursively convert children to types/nodes
            let fields = fields
                .iter()
                .map(|f| to_parquet_type_with_options(f, decimal256_max))
                .collect::<Result<Vec<_>>>()?;
            Ok(ParquetType::from_group(
                name, repetition, None, None, fields, None,
            ))
        }
        DataType::Dictionary(_, value, _) => {
            let dict_field = Field::new(name.as_str(), value.as_ref().clone(), field.is_nullable);
            to_parquet_type_with_options(&dict_field, decimal256_max)
        }
        DataType::FixedSizeBinary(size) => Ok(ParquetType::try_from_primitive(
            name,
            PhysicalType::FixedLenByteArray(*size),
            repetition,
            None,
            None,
            None,
        )?),
        DataType::Decimal(precision, scale) => {
            let precision = *precision;
            let scale = *scale;
            let logical_type = Some(PrimitiveLogicalType::Decimal(precision, scale));

            let physical_type = if precision <= 9 {
                PhysicalType::Int32
            } else if precision <= 18 {
                PhysicalType::Int64
            } else {
                let len = decimal_length_from_precision(precision);
                PhysicalType::FixedLenByteArray(len)
            };
            Ok(ParquetType::try_from_primitive(
                name,
                physical_type,
                repetition,
                Some(PrimitiveConvertedType::Decimal(precision, scale)),
                logical_type,
                None,
            )?)
        }
        DataType::Decimal256(precision, scale) => {
            let precision = *precision;
            let scale = *scale;
            let logical_type = Some(PrimitiveLogicalType::Decimal(precision, scale));

            if precision <= 9 {
                Ok(ParquetType::try_from_primitive(
                    name,
                    PhysicalType::Int32,
                    repetition,
                    Some(PrimitiveConvertedType::Decimal(precision, scale)),
                    logical_type,
                    None,
                )?)
            } else if precision <= 18 {
                Ok(ParquetType::try_from_primitive(
                    name,
                    PhysicalType::Int64,
                    repetition,
                    Some(PrimitiveConvertedType::Decimal(precision, scale)),
                    logical_type,
                    None,
                )?)
            } else if precision <= 38 {
                let len = decimal_length_from_precision(precision);
                Ok(ParquetType::try_from_primitive(
                    name,
                    PhysicalType::FixedLenByteArray(len),
                    repetition,
                    Some(PrimitiveConvertedType::Decimal(precision, scale)),
                    logical_type,
                    None,
                )?)
            } else {
                if decimal256_max {
                    Ok(ParquetType::try_from_primitive(
                        name,
                        PhysicalType::FixedLenByteArray(32),
                        repetition,
                        Some(PrimitiveConvertedType::Decimal(precision, scale)),
                        logical_type,
                        None,
                    )?)
                } else {
                    let len = decimal_length_from_precision(precision);
                    Ok(ParquetType::try_from_primitive(
                        name,
                        PhysicalType::FixedLenByteArray(len),
                        repetition,
                        Some(PrimitiveConvertedType::Decimal(precision, scale)),
                        logical_type,
                        None,
                    )?)
                }
            }
        }
        DataType::Interval(_) => Ok(ParquetType::try_from_primitive(
            name,
            PhysicalType::FixedLenByteArray(12),
            repetition,
            Some(PrimitiveConvertedType::Interval),
            None,
            None,
        )?),
        DataType::List(f) | DataType::FixedSizeList(f, _) | DataType::LargeList(f) => {
            Ok(ParquetType::from_group(
                name,
                repetition,
                Some(GroupConvertedType::List),
                Some(GroupLogicalType::List),
                vec![ParquetType::from_group(
                    "list".to_string(),
                    Repetition::Repeated,
                    None,
                    None,
                    vec![to_parquet_type_with_options(f, decimal256_max)?],
                    None,
                )],
                None,
            ))
        }
        DataType::Map(f, _) => Ok(ParquetType::from_group(
            name,
            repetition,
            Some(GroupConvertedType::Map),
            Some(GroupLogicalType::Map),
            vec![ParquetType::from_group(
                "map".to_string(),
                Repetition::Repeated,
                None,
                None,
                vec![to_parquet_type_with_options(f, decimal256_max)?],
                None,
            )],
            None,
        )),
        other => Err(Error::NotYetImplemented(format!(
            "Writing the data type {other:?} is not yet implemented"
        ))),
    }
}
