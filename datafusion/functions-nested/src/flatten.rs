// Licensed to the Apache Software Foundation (ASF) under one
// or more contributor license agreements.  See the NOTICE file
// distributed with this work for additional information
// regarding copyright ownership.  The ASF licenses this file
// to you under the Apache License, Version 2.0 (the
// "License"); you may not use this file except in compliance
// with the License.  You may obtain a copy of the License at
//
//   http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing,
// software distributed under the License is distributed on an
// "AS IS" BASIS, WITHOUT WARRANTIES OR CONDITIONS OF ANY
// KIND, either express or implied.  See the License for the
// specific language governing permissions and limitations
// under the License.

//! [`ScalarUDFImpl`] definitions for flatten function.

use crate::utils::make_scalar_function;
use arrow::array::{ArrayRef, GenericListArray, OffsetSizeTrait};
use arrow::buffer::OffsetBuffer;
use arrow::datatypes::{
    DataType,
    DataType::{FixedSizeList, LargeList, List, Null},
};
use datafusion_common::cast::{
    as_generic_list_array, as_large_list_array, as_list_array,
};
use datafusion_common::{exec_err, utils::take_function_args, Result};
use datafusion_expr::{
    ArrayFunctionSignature, ColumnarValue, Documentation, ScalarUDFImpl, Signature,
    TypeSignature, Volatility,
};
use datafusion_macros::user_doc;
use std::any::Any;
use std::sync::Arc;

make_udf_expr_and_func!(
    Flatten,
    flatten,
    array,
    "flattens an array of arrays into a single array.",
    flatten_udf
);

#[user_doc(
    doc_section(label = "Array Functions"),
    description = "Converts an array of arrays to a flat array.\n\n- Applies to any depth of nested arrays\n- Does not change arrays that are already flat\n\nThe flattened array contains all the elements from all source arrays.",
    syntax_example = "flatten(array)",
    sql_example = r#"```sql
> select flatten([[1, 2], [3, 4]]);
+------------------------------+
| flatten(List([1,2], [3,4]))  |
+------------------------------+
| [1, 2, 3, 4]                 |
+------------------------------+
```"#,
    argument(
        name = "array",
        description = "Array expression. Can be a constant, column, or function, and any combination of array operators."
    )
)]
#[derive(Debug)]
pub struct Flatten {
    signature: Signature,
    aliases: Vec<String>,
}

impl Default for Flatten {
    fn default() -> Self {
        Self::new()
    }
}

impl Flatten {
    pub fn new() -> Self {
        Self {
            signature: Signature {
                // TODO (https://github.com/apache/datafusion/issues/13757) flatten should be single-step, not recursive
                type_signature: TypeSignature::ArraySignature(
                    ArrayFunctionSignature::RecursiveArray,
                ),
                volatility: Volatility::Immutable,
            },
            aliases: vec![],
        }
    }
}

impl ScalarUDFImpl for Flatten {
    fn as_any(&self) -> &dyn Any {
        self
    }

    fn name(&self) -> &str {
        "flatten"
    }

    fn signature(&self) -> &Signature {
        &self.signature
    }

    fn return_type(&self, arg_types: &[DataType]) -> Result<DataType> {
        fn get_base_type(data_type: &DataType) -> Result<DataType> {
            match data_type {
                List(field) | FixedSizeList(field, _)
                    if matches!(field.data_type(), List(_) | FixedSizeList(_, _)) =>
                {
                    get_base_type(field.data_type())
                }
                LargeList(field) if matches!(field.data_type(), LargeList(_)) => {
                    get_base_type(field.data_type())
                }
                Null | List(_) | LargeList(_) => Ok(data_type.to_owned()),
                FixedSizeList(field, _) => Ok(List(Arc::clone(field))),
                _ => exec_err!(
                    "Not reachable, data_type should be List, LargeList or FixedSizeList"
                ),
            }
        }

        let data_type = get_base_type(&arg_types[0])?;
        Ok(data_type)
    }

    fn invoke_with_args(
        &self,
        args: datafusion_expr::ScalarFunctionArgs,
    ) -> Result<ColumnarValue> {
        make_scalar_function(flatten_inner)(&args.args)
    }

    fn aliases(&self) -> &[String] {
        &self.aliases
    }

    fn documentation(&self) -> Option<&Documentation> {
        self.doc()
    }
}

/// Flatten SQL function
pub fn flatten_inner(args: &[ArrayRef]) -> Result<ArrayRef> {
    let [array] = take_function_args("flatten", args)?;

    match array.data_type() {
        List(_) => {
            let list_arr = as_list_array(&array)?;
            let flattened_array = flatten_internal::<i32>(list_arr.clone(), None)?;
            Ok(Arc::new(flattened_array) as ArrayRef)
        }
        LargeList(_) => {
            let list_arr = as_large_list_array(&array)?;
            let flattened_array = flatten_internal::<i64>(list_arr.clone(), None)?;
            Ok(Arc::new(flattened_array) as ArrayRef)
        }
        Null => Ok(Arc::clone(array)),
        _ => {
            exec_err!("flatten does not support type '{:?}'", array.data_type())
        }
    }
}

fn flatten_internal<O: OffsetSizeTrait>(
    list_arr: GenericListArray<O>,
    indexes: Option<OffsetBuffer<O>>,
) -> Result<GenericListArray<O>> {
    let (field, offsets, values, _) = list_arr.clone().into_parts();
    let data_type = field.data_type();

    match data_type {
        // Recursively get the base offsets for flattened array
        List(_) | LargeList(_) => {
            let sub_list = as_generic_list_array::<O>(&values)?;
            if let Some(indexes) = indexes {
                let offsets = get_offsets_for_flatten(offsets, indexes);
                flatten_internal::<O>(sub_list.clone(), Some(offsets))
            } else {
                flatten_internal::<O>(sub_list.clone(), Some(offsets))
            }
        }
        // Reach the base level, create a new list array
        _ => {
            if let Some(indexes) = indexes {
                let offsets = get_offsets_for_flatten(offsets, indexes);
                let list_arr = GenericListArray::<O>::new(field, offsets, values, None);
                Ok(list_arr)
            } else {
                Ok(list_arr)
            }
        }
    }
}

// Create new offsets that are equivalent to `flatten` the array.
fn get_offsets_for_flatten<O: OffsetSizeTrait>(
    offsets: OffsetBuffer<O>,
    indexes: OffsetBuffer<O>,
) -> OffsetBuffer<O> {
    let buffer = offsets.into_inner();
    let offsets: Vec<O> = indexes
        .iter()
        .map(|i| buffer[i.to_usize().unwrap()])
        .collect();
    OffsetBuffer::new(offsets.into())
}
