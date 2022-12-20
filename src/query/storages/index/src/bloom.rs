// Copyright 2021 Datafuse Labs.
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

use std::collections::HashMap;
use std::sync::Arc;

use common_catalog::table_context::TableContext;
use common_exception::ErrorCode;
use common_exception::Result;
use common_expression::types::DataType;
use common_expression::Chunk;
use common_expression::ChunkEntry;
use common_expression::ColumnIndex;
use common_expression::ConstantFolder;
use common_expression::DataSchemaRef;
use common_expression::Domain;
use common_expression::Expr;
use common_expression::FunctionContext;
use common_expression::Scalar;
use common_expression::Span;
use common_expression::Value;
use common_functions_v2::scalars::BUILTIN_FUNCTIONS;

use crate::filters::Filter;
use crate::filters::FilterBuilder;
use crate::filters::Xor8Builder;
use crate::filters::Xor8Filter;
use crate::SupportedType;

/// ChunkFilter represents multiple per-column filters(bloom filter or xor filter etc) for chunk.
///
/// By default we create a filter per column for a parquet data file. For columns whose data_type
/// are not applicable for a filter, we skip the creation.
/// That is to say, it is legal to have a ChunkFilter with zero columns.
///
/// For example, for the source chunk as follows:
/// ```
///         +---name--+--age--+
///         | "Alice" |  20   |
///         | "Bob"   |  30   |
///         +---------+-------+
/// ```
/// We will create table of filters as follows:
/// ```
///         +---Bloom(name)--+--Bloom(age)--+
///         |  123456789abcd |  ac2345bcd   |
///         +----------------+--------------+
/// ```
pub struct ChunkFilter {
    // /// The schema of the source table/chunk, which the filter work for.
    // pub source_schema: DataSchemaRef,
    /// Data chunk of filters.
    pub filter_chunk: Chunk<String>,
    pub fn_ctx: FunctionContext,
    /// Approximate distinct count of columns generated by xor hash function.
    pub column_distinct_count: HashMap<usize, usize>,
}

/// FilterExprEvalResult represents the evaluation result of an expression by a filter.
///
/// For example, expression of 'age = 12' should return false is the filter are sure
/// of the nonexistent of value '12' in column 'age'. Otherwise should return 'Maybe'.
///
/// If the column is not applicable for a filter, like TypeID::struct, Uncertain is used.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FilterEvalResult {
    MustFalse,
    Uncertain,
}

impl ChunkFilter {
    /// Load a filter directly from the source table's schema and the corresponding filter parquet file.
    #[tracing::instrument(level = "debug", skip_all)]
    pub fn from_filter_chunk(fn_ctx: FunctionContext, filter_chunk: Chunk<String>) -> Result<Self> {
        Ok(Self {
            filter_chunk,
            fn_ctx,
            column_distinct_count: HashMap::new(),
        })
    }

    /// Create a filter chunk from source data.
    ///
    /// All input chunks should belong to a Parquet file, e.g. the chunk array represents the parquet file in memory.
    pub fn try_create(fn_ctx: FunctionContext, chunks: &[&Chunk<String>]) -> Result<Self> {
        if chunks.is_empty() {
            return Err(ErrorCode::BadArguments("chunks is empty"));
        }

        let mut filter_columns = vec![];
        let mut column_distinct_count = HashMap::<usize, usize>::new();

        for i in 0..chunks[0].num_columns() {
            if Xor8Filter::is_supported_type(&chunks[0].get_by_offset(i).data_type) {
                // create filter per column
                let mut filter_builder = Xor8Builder::create();

                // ingest the same column data from all chunks
                for chunk in chunks {
                    let entry = chunk.get_by_offset(i);
                    match &entry.value {
                        Value::Scalar(scalar) => filter_builder.add_key(&scalar),
                        Value::Column(col) => {
                            col.iter()
                                .for_each(|scalar| filter_builder.add_key(&scalar));
                        }
                    }
                }

                let filter = filter_builder.build()?;
                if let Some(len) = filter.len() {
                    column_distinct_count.insert(i, len);
                }

                // create filter column
                let serialized_bytes = filter.to_bytes()?;
                let filter_value = Value::Scalar(Scalar::String(serialized_bytes));
                filter_columns.push(ChunkEntry {
                    id: Self::build_filter_column_name(&chunks[0].get_by_offset(i).id),
                    data_type: DataType::String,
                    value: filter_value,
                });
            }
        }

        let filter_chunk = Chunk::new(filter_columns, 1);

        Ok(Self {
            filter_chunk,
            fn_ctx,
            column_distinct_count,
        })
    }

    /// Apply the predicate expression, return the result.
    /// If we are sure of skipping the scan, return false, e.g. the expression must be false.
    /// This happens when the data doesn't show up in the filter.
    ///
    /// Otherwise return `Uncertain`.
    #[tracing::instrument(level = "debug", name = "block_filter_index_eval", skip_all)]
    pub fn eval(&self, mut expr: Expr<String>) -> Result<FilterEvalResult> {
        visit_expr_column_eq_constant(&mut expr, &mut |span, col_name, scalar, ty| {
            // If the column doesn't contain the constant, we rewrite the expression to `false`.
            if self.find(col_name, &scalar, ty)? == FilterEvalResult::MustFalse {
                Ok(Some(Expr::Constant {
                    span,
                    scalar: Scalar::Boolean(false),
                    data_type: DataType::Boolean,
                }))
            } else {
                Ok(None)
            }
        })?;

        let input_domains = expr
            .column_refs()
            .into_iter()
            .map(|(name, ty)| {
                let domain = Domain::full(&ty);
                (name, domain)
            })
            .collect();
        let folder = ConstantFolder::new(input_domains, self.fn_ctx, &BUILTIN_FUNCTIONS);
        let (new_expr, _) = folder.fold(&expr);

        match new_expr {
            Expr::Constant {
                scalar: Scalar::Boolean(false),
                ..
            } => Ok(FilterEvalResult::MustFalse),
            _ => Ok(FilterEvalResult::Uncertain),
        }
    }

    /// Find all columns that match the pattern of `col = <constant>` in the expression.
    pub fn find_eq_columns(expr: &Expr<String>) -> Result<Vec<String>> {
        let mut cols = Vec::new();
        visit_expr_column_eq_constant(&mut expr.clone(), &mut |_, col_name, _, _| {
            cols.push(col_name.to_string());
            Ok(None)
        })?;
        Ok(cols)
    }

    /// For every applicable column, we will create a filter.
    /// The filter will be stored with field name 'Bloom(column_name)'
    pub fn build_filter_column_name(column_name: &str) -> String {
        format!("Bloom({})", column_name)
    }

    fn find(&self, column_name: &str, target: &Scalar, ty: &DataType) -> Result<FilterEvalResult> {
        let filter_column = Self::build_filter_column_name(column_name);
        if !Xor8Filter::is_supported_type(ty) || target.is_null() {
            // The column doesn't have a filter.
            return Ok(FilterEvalResult::Uncertain);
        }

        match self.filter_chunk.get_by_id(&filter_column) {
            Some(entry) => {
                let filter_bytes = entry.value.as_scalar().unwrap().as_string().unwrap();
                let (filter, _size) = Xor8Filter::from_bytes(&filter_bytes)?;
                if filter.contains(&target) {
                    Ok(FilterEvalResult::Uncertain)
                } else {
                    Ok(FilterEvalResult::MustFalse)
                }
            }
            None => Ok(FilterEvalResult::Uncertain),
        }
    }
}

fn visit_expr_column_eq_constant(
    expr: &mut Expr<String>,
    visitor: &mut impl FnMut(Span, &str, &Scalar, &DataType) -> Result<Option<Expr<String>>>,
) -> Result<()> {
    // Find patterns like `Column = <constant>` or `<constant> = Column`.
    match expr {
        Expr::FunctionCall {
            span,
            function,
            args,
            ..
        } if function.signature.name == "eq" => match args.as_slice() {
            [
                Expr::ColumnRef { id, data_type, .. },
                Expr::Constant { scalar, .. },
            ]
            | [
                Expr::Constant { scalar, .. },
                Expr::ColumnRef { id, data_type, .. },
            ] => {
                // If the visitor returns a new expression, replace the current expression.
                if let Some(new_expr) = visitor(span.clone(), id, scalar, data_type)? {
                    *expr = new_expr;
                    return Ok(());
                }
            }
            _ => (),
        },
        _ => (),
    }

    // Otherwise, rewrite sub expressions.
    match expr {
        Expr::Cast { expr, .. } => {
            visit_expr_column_eq_constant(expr, visitor)?;
        }
        Expr::FunctionCall { args, .. } => {
            for arg in args.iter_mut() {
                visit_expr_column_eq_constant(arg, visitor)?;
            }
        }
        _ => (),
    }

    Ok(())
}
