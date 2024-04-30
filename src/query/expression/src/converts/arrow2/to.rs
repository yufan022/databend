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

use databend_common_arrow::arrow::bitmap::Bitmap;
use databend_common_arrow::arrow::buffer::Buffer;
use databend_common_arrow::arrow::datatypes::DataType as ArrowDataType;
use databend_common_arrow::arrow::datatypes::Field as ArrowField;
use databend_common_arrow::arrow::datatypes::Schema as ArrowSchema;
use databend_common_arrow::arrow::datatypes::TimeUnit;
use databend_common_arrow::arrow::offset::OffsetsBuffer;

use super::ARROW_EXT_TYPE_BITMAP;
use super::ARROW_EXT_TYPE_EMPTY_ARRAY;
use super::ARROW_EXT_TYPE_EMPTY_MAP;
use super::ARROW_EXT_TYPE_VARIANT;
use crate::types::decimal::DecimalColumn;
use crate::types::DecimalDataType;
use crate::types::NumberColumn;
use crate::types::NumberDataType;
use crate::types::F32;
use crate::types::F64;
use crate::with_number_type;
use crate::Column;
use crate::DataField;
use crate::DataSchema;
use crate::TableDataType;
use crate::TableField;
use crate::TableSchema;

impl From<&TableSchema> for ArrowSchema {
    fn from(schema: &TableSchema) -> Self {
        let fields = schema
            .fields()
            .iter()
            .map(ArrowField::from)
            .collect::<Vec<_>>();
        ArrowSchema::from(fields).with_metadata(schema.metadata.clone())
    }
}

impl From<&DataSchema> for ArrowSchema {
    fn from(schema: &DataSchema) -> Self {
        let fields = schema
            .fields()
            .iter()
            .map(ArrowField::from)
            .collect::<Vec<_>>();
        ArrowSchema::from(fields).with_metadata(schema.metadata.clone())
    }
}

impl From<&TableField> for ArrowField {
    fn from(f: &TableField) -> Self {
        let ty = table_type_to_arrow_type(&f.data_type, false);
        ArrowField::new(f.name(), ty, f.is_nullable())
    }
}

impl From<&DataField> for ArrowField {
    fn from(f: &DataField) -> Self {
        ArrowField::from(&TableField::from(f))
    }
}

// Note: Arrow's data type is not nullable, so we need to explicitly
// add nullable information to Arrow's field afterwards.
fn table_type_to_arrow_type(ty: &TableDataType, inside_nullable: bool) -> ArrowDataType {
    match ty {
        TableDataType::Null => ArrowDataType::Null,
        TableDataType::EmptyArray => ArrowDataType::Extension(
            ARROW_EXT_TYPE_EMPTY_ARRAY.to_string(),
            Box::new(ArrowDataType::Null),
            None,
        ),
        TableDataType::EmptyMap => ArrowDataType::Extension(
            ARROW_EXT_TYPE_EMPTY_MAP.to_string(),
            Box::new(ArrowDataType::Null),
            None,
        ),
        TableDataType::Boolean => ArrowDataType::Boolean,
        TableDataType::Binary => ArrowDataType::LargeBinary,
        TableDataType::String => ArrowDataType::LargeUtf8,
        TableDataType::Number(ty) => with_number_type!(|TYPE| match ty {
            NumberDataType::TYPE => ArrowDataType::TYPE,
        }),
        TableDataType::Decimal(DecimalDataType::Decimal128(size)) => {
            ArrowDataType::Decimal(size.precision as usize, size.scale as usize)
        }
        TableDataType::Decimal(DecimalDataType::Decimal256(size)) => {
            ArrowDataType::Decimal256(size.precision as usize, size.scale as usize)
        }
        TableDataType::Timestamp => ArrowDataType::Timestamp(TimeUnit::Microsecond, None),
        TableDataType::Date => ArrowDataType::Date32,
        TableDataType::Nullable(ty) => table_type_to_arrow_type(ty.as_ref(), true),
        TableDataType::Array(ty) => {
            let arrow_ty = table_type_to_arrow_type(ty.as_ref(), inside_nullable);
            ArrowDataType::LargeList(Box::new(ArrowField::new(
                "_array",
                arrow_ty,
                ty.is_nullable(),
            )))
        }
        TableDataType::Map(ty) => {
            let inner_ty = match ty.as_ref() {
                TableDataType::Tuple {
                    fields_name: _fields_name,
                    fields_type,
                } => {
                    let key_ty = table_type_to_arrow_type(&fields_type[0], inside_nullable);
                    let val_ty = table_type_to_arrow_type(&fields_type[1], inside_nullable);
                    let key_field = ArrowField::new("key", key_ty, fields_type[0].is_nullable());
                    let val_field = ArrowField::new("value", val_ty, fields_type[1].is_nullable());
                    ArrowDataType::Struct(vec![key_field, val_field])
                }
                _ => unreachable!(),
            };
            ArrowDataType::Map(
                Box::new(ArrowField::new("entries", inner_ty, ty.is_nullable())),
                false,
            )
        }
        TableDataType::Bitmap => ArrowDataType::Extension(
            ARROW_EXT_TYPE_BITMAP.to_string(),
            Box::new(ArrowDataType::LargeBinary),
            None,
        ),
        TableDataType::Tuple {
            fields_name,
            fields_type,
        } => {
            let fields = fields_name
                .iter()
                .zip(fields_type)
                .map(|(name, ty)| {
                    ArrowField::new(
                        name.as_str(),
                        table_type_to_arrow_type(ty, inside_nullable),
                        ty.is_nullable() || inside_nullable,
                    )
                })
                .collect();
            ArrowDataType::Struct(fields)
        }
        TableDataType::Variant => ArrowDataType::Extension(
            ARROW_EXT_TYPE_VARIANT.to_string(),
            Box::new(ArrowDataType::LargeBinary),
            None,
        ),
    }
}

impl Column {
    pub fn arrow_field(&self) -> ArrowField {
        ArrowField::from(&DataField::new("DUMMY", self.data_type()))
    }

    pub fn as_arrow(&self) -> Box<dyn databend_common_arrow::arrow::array::Array> {
        let arrow_type = self.arrow_field().data_type;
        match self {
            Column::Null { len } => Box::new(
                databend_common_arrow::arrow::array::NullArray::new_null(arrow_type, *len),
            ),
            Column::EmptyArray { len } => Box::new(
                databend_common_arrow::arrow::array::NullArray::new_null(arrow_type, *len),
            ),
            Column::EmptyMap { len } => Box::new(
                databend_common_arrow::arrow::array::NullArray::new_null(arrow_type, *len),
            ),
            Column::Number(NumberColumn::UInt8(col)) => Box::new(
                databend_common_arrow::arrow::array::PrimitiveArray::<u8>::try_new(
                    arrow_type,
                    col.clone(),
                    None,
                )
                .unwrap(),
            ),
            Column::Number(NumberColumn::UInt16(col)) => Box::new(
                databend_common_arrow::arrow::array::PrimitiveArray::<u16>::try_new(
                    arrow_type,
                    col.clone(),
                    None,
                )
                .unwrap(),
            ),
            Column::Number(NumberColumn::UInt32(col)) => Box::new(
                databend_common_arrow::arrow::array::PrimitiveArray::<u32>::try_new(
                    arrow_type,
                    col.clone(),
                    None,
                )
                .unwrap(),
            ),
            Column::Number(NumberColumn::UInt64(col)) => Box::new(
                databend_common_arrow::arrow::array::PrimitiveArray::<u64>::try_new(
                    arrow_type,
                    col.clone(),
                    None,
                )
                .unwrap(),
            ),
            Column::Number(NumberColumn::Int8(col)) => Box::new(
                databend_common_arrow::arrow::array::PrimitiveArray::<i8>::try_new(
                    arrow_type,
                    col.clone(),
                    None,
                )
                .unwrap(),
            ),
            Column::Number(NumberColumn::Int16(col)) => Box::new(
                databend_common_arrow::arrow::array::PrimitiveArray::<i16>::try_new(
                    arrow_type,
                    col.clone(),
                    None,
                )
                .unwrap(),
            ),
            Column::Number(NumberColumn::Int32(col)) => Box::new(
                databend_common_arrow::arrow::array::PrimitiveArray::<i32>::try_new(
                    arrow_type,
                    col.clone(),
                    None,
                )
                .unwrap(),
            ),
            Column::Number(NumberColumn::Int64(col)) => Box::new(
                databend_common_arrow::arrow::array::PrimitiveArray::<i64>::try_new(
                    arrow_type,
                    col.clone(),
                    None,
                )
                .unwrap(),
            ),
            Column::Number(NumberColumn::Float32(col)) => {
                let values =
                    unsafe { std::mem::transmute::<Buffer<F32>, Buffer<f32>>(col.clone()) };
                Box::new(
                    databend_common_arrow::arrow::array::PrimitiveArray::<f32>::try_new(
                        arrow_type, values, None,
                    )
                    .unwrap(),
                )
            }
            Column::Number(NumberColumn::Float64(col)) => {
                let values =
                    unsafe { std::mem::transmute::<Buffer<F64>, Buffer<f64>>(col.clone()) };
                Box::new(
                    databend_common_arrow::arrow::array::PrimitiveArray::<f64>::try_new(
                        arrow_type, values, None,
                    )
                    .unwrap(),
                )
            }
            Column::Decimal(DecimalColumn::Decimal128(col, _)) => Box::new(
                databend_common_arrow::arrow::array::PrimitiveArray::<i128>::try_new(
                    arrow_type,
                    col.clone(),
                    None,
                )
                .unwrap(),
            ),
            Column::Decimal(DecimalColumn::Decimal256(col, _)) => {
                let values = unsafe { std::mem::transmute(col.clone()) };
                Box::new(
                    databend_common_arrow::arrow::array::PrimitiveArray::<
                        databend_common_arrow::arrow::types::i256,
                    >::try_new(arrow_type, values, None)
                    .unwrap(),
                )
            }
            Column::Boolean(col) => Box::new(
                databend_common_arrow::arrow::array::BooleanArray::try_new(
                    arrow_type,
                    col.clone(),
                    None,
                )
                .unwrap(),
            ),
            Column::Binary(col) => {
                let offsets: Buffer<i64> =
                    col.offsets().iter().map(|offset| *offset as i64).collect();
                Box::new(
                    databend_common_arrow::arrow::array::BinaryArray::<i64>::try_new(
                        arrow_type,
                        unsafe { OffsetsBuffer::new_unchecked(offsets) },
                        col.data().clone(),
                        None,
                    )
                    .unwrap(),
                )
            }
            Column::String(col) => {
                let offsets: Buffer<i64> =
                    col.offsets().iter().map(|offset| *offset as i64).collect();

                unsafe {
                    Box::new(
                        databend_common_arrow::arrow::array::Utf8Array::<i64>::try_new_unchecked(
                            arrow_type,
                            OffsetsBuffer::new_unchecked(offsets),
                            col.data().clone(),
                            None,
                        )
                        .unwrap(),
                    )
                }
            }
            Column::Timestamp(col) => Box::new(
                databend_common_arrow::arrow::array::PrimitiveArray::<i64>::try_new(
                    arrow_type,
                    col.clone(),
                    None,
                )
                .unwrap(),
            ),
            Column::Date(col) => Box::new(
                databend_common_arrow::arrow::array::PrimitiveArray::<i32>::try_new(
                    arrow_type,
                    col.clone(),
                    None,
                )
                .unwrap(),
            ),
            Column::Array(col) => {
                let offsets: Buffer<i64> =
                    col.offsets.iter().map(|offset| *offset as i64).collect();
                Box::new(
                    databend_common_arrow::arrow::array::ListArray::<i64>::try_new(
                        arrow_type,
                        unsafe { OffsetsBuffer::new_unchecked(offsets) },
                        col.values.as_arrow(),
                        None,
                    )
                    .unwrap(),
                )
            }
            Column::Map(col) => {
                let offsets: Buffer<i32> =
                    col.offsets.iter().map(|offset| *offset as i32).collect();
                let values = match (&arrow_type, &col.values) {
                    (ArrowDataType::Map(inner_field, _), Column::Tuple(fields)) => {
                        let inner_type = inner_field.data_type.clone();
                        Box::new(
                            databend_common_arrow::arrow::array::StructArray::try_new(
                                inner_type,
                                fields.iter().map(|field| field.as_arrow()).collect(),
                                None,
                            )
                            .unwrap(),
                        )
                    }
                    (_, _) => unreachable!(),
                };
                Box::new(
                    databend_common_arrow::arrow::array::MapArray::try_new(
                        arrow_type,
                        unsafe { OffsetsBuffer::new_unchecked(offsets) },
                        values,
                        None,
                    )
                    .unwrap(),
                )
            }
            Column::Bitmap(col) => {
                let offsets: Buffer<i64> =
                    col.offsets().iter().map(|offset| *offset as i64).collect();
                Box::new(
                    databend_common_arrow::arrow::array::BinaryArray::<i64>::try_new(
                        arrow_type,
                        unsafe { OffsetsBuffer::new_unchecked(offsets) },
                        col.data().clone(),
                        None,
                    )
                    .unwrap(),
                )
            }
            Column::Nullable(col) => {
                let arrow_array = col.column.as_arrow();
                set_validities(arrow_array.clone(), &col.validity)
            }
            Column::Tuple(fields) => Box::new(
                databend_common_arrow::arrow::array::StructArray::try_new(
                    arrow_type,
                    fields.iter().map(|field| field.as_arrow()).collect(),
                    None,
                )
                .unwrap(),
            ),
            Column::Variant(col) => {
                let offsets: Buffer<i64> =
                    col.offsets().iter().map(|offset| *offset as i64).collect();
                Box::new(
                    databend_common_arrow::arrow::array::BinaryArray::<i64>::try_new(
                        arrow_type,
                        unsafe { OffsetsBuffer::new_unchecked(offsets) },
                        col.data().clone(),
                        None,
                    )
                    .unwrap(),
                )
            }
        }
    }
}

pub fn set_validities(
    arrow_array: Box<dyn databend_common_arrow::arrow::array::Array>,
    validity: &Bitmap,
) -> Box<dyn databend_common_arrow::arrow::array::Array> {
    // merge Struct validity with the inner fields validity
    let validity = match arrow_array.validity() {
        Some(inner_validity) => inner_validity & validity,
        None => validity.clone(),
    };

    match arrow_array.data_type() {
        ArrowDataType::Null => arrow_array.clone(),
        ArrowDataType::Extension(_, t, _) if **t == ArrowDataType::Null => arrow_array.clone(),
        ArrowDataType::Struct(_) => {
            let struct_array = arrow_array
                .as_any()
                .downcast_ref::<databend_common_arrow::arrow::array::StructArray>()
                .expect("fail to read from arrow: array should be `StructArray`");
            let fields = struct_array
                .values()
                .iter()
                .map(|array| {
                    let array = set_validities(array.clone(), &validity);
                    array.clone()
                })
                .collect::<Vec<_>>();
            Box::new(
                databend_common_arrow::arrow::array::StructArray::try_new(
                    arrow_array.data_type().clone(),
                    fields,
                    Some(validity),
                )
                .unwrap(),
            )
        }
        _ => arrow_array.with_validity(Some(validity)),
    }
}
