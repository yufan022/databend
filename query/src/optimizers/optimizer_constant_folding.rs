// Copyright 2020 Datafuse Labs.
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

use common_datablocks::DataBlock;
use common_datavalues::chrono::NaiveDate;
use common_datavalues::chrono::NaiveDateTime;
use common_datavalues::prelude::*;
use common_exception::ErrorCode;
use common_exception::Result;
use common_functions::scalars::FunctionFactory;
use common_planners::AggregatorFinalPlan;
use common_planners::AggregatorPartialPlan;
use common_planners::Expression;
use common_planners::Expressions;
use common_planners::PlanBuilder;
use common_planners::PlanNode;
use common_planners::PlanRewriter;

use crate::optimizers::Optimizer;
use crate::pipelines::transforms::ExpressionExecutor;
use crate::sessions::DatabendQueryContextRef;

pub struct ConstantFoldingOptimizer {}

struct ConstantFoldingImpl {
    before_group_by_schema: Option<DataSchemaRef>,
}

impl ConstantFoldingImpl {
    fn rewrite_alias(alias: &str, expr: Expression) -> Result<Expression> {
        Ok(Expression::Alias(alias.to_string(), Box::new(expr)))
    }

    fn constants_arguments(args: &[Expression]) -> bool {
        !args
            .iter()
            .any(|expr| !matches!(expr, Expression::Literal { .. }))
    }

    fn rewrite_function<F>(op: &str, args: Expressions, name: String, f: F) -> Result<Expression>
    where F: Fn(&str, Expressions) -> Expression {
        let factory = FunctionFactory::instance();
        let function_features = factory.get_features(op)?;

        if function_features.is_deterministic && Self::constants_arguments(&args) {
            let op = op.to_string();
            return ConstantFoldingImpl::execute_expression(
                Expression::ScalarFunction { op, args },
                name,
            );
        }

        Ok(f(op, args))
    }

    fn expr_executor(schema: &DataSchemaRef, expr: Expression) -> Result<ExpressionExecutor> {
        let output_fields = vec![expr.to_data_field(schema)?];
        let output_schema = DataSchemaRefExt::create(output_fields);
        ExpressionExecutor::try_create(
            "Constant folding optimizer.",
            schema.clone(),
            output_schema,
            vec![expr],
            false,
        )
    }

    fn execute_expression(expression: Expression, origin_name: String) -> Result<Expression> {
        let input_fields = vec![DataField::new("_dummy", DataType::UInt8, false)];
        let input_schema = Arc::new(DataSchema::new(input_fields));

        let data_type = expression.to_data_type(&input_schema)?;
        let expression_executor = Self::expr_executor(&input_schema, expression)?;
        let dummy_columns = vec![DataColumn::Constant(DataValue::UInt8(Some(1)), 1)];
        let data_block = DataBlock::create(input_schema, dummy_columns);
        let executed_data_block = expression_executor.execute(&data_block)?;

        ConstantFoldingImpl::convert_to_expression(origin_name, executed_data_block, data_type)
    }

    fn convert_to_expression(
        column_name: String,
        data_block: DataBlock,
        data_type: DataType,
    ) -> Result<Expression> {
        debug_assert!(data_block.num_rows() == 1);
        debug_assert!(data_block.num_columns() == 1);

        let column_name = Some(column_name);
        let value = data_block.column(0).try_get(0)?;
        Ok(Expression::Literal {
            value,
            column_name,
            data_type,
        })
    }

    fn remove_const_cond(
        &mut self,
        schema: &DataSchemaRef,
        column_name: String,
        left: &Expression,
        right: &Expression,
        is_and: bool,
    ) -> Result<Expression> {
        let mut is_remove = false;

        let mut left_const = false;
        let new_left = self.eval_const_cond(
            schema,
            column_name.clone(),
            left,
            is_and,
            &mut left_const,
            &mut is_remove,
        )?;
        if is_remove {
            return Ok(new_left);
        }

        let mut right_const = false;
        let new_right = self.eval_const_cond(
            schema,
            column_name.clone(),
            right,
            is_and,
            &mut right_const,
            &mut is_remove,
        )?;
        if is_remove {
            return Ok(new_right);
        }

        match (left_const, right_const) {
            (true, true) => {
                if is_and {
                    Ok(Expression::Literal {
                        value: DataValue::Boolean(Some(true)),
                        column_name: Some(column_name),
                        data_type: DataType::Boolean,
                    })
                } else {
                    Ok(Expression::Literal {
                        value: DataValue::Boolean(Some(false)),
                        column_name: Some(column_name),
                        data_type: DataType::Boolean,
                    })
                }
            }
            (true, false) => Ok(new_right),
            (false, true) => Ok(new_left),
            (false, false) => {
                if is_and {
                    Ok(new_left.and(new_right))
                } else {
                    Ok(new_left.or(new_right))
                }
            }
        }
    }

    fn eval_const_cond(
        &mut self,
        schema: &DataSchemaRef,
        column_name: String,
        expr: &Expression,
        is_and: bool,
        is_const: &mut bool,
        is_remove: &mut bool,
    ) -> Result<Expression> {
        let new_expr = self.rewrite_expr(schema, expr)?;
        match new_expr {
            Expression::Literal { ref value, .. } => {
                *is_const = true;
                let val = value.as_bool()?;
                if val {
                    if !is_and {
                        *is_remove = true;
                        return Ok(Expression::Literal {
                            value: DataValue::Boolean(Some(true)),
                            column_name: Some(column_name),
                            data_type: DataType::Boolean,
                        });
                    }
                } else if is_and {
                    *is_remove = true;
                    return Ok(Expression::Literal {
                        value: DataValue::Boolean(Some(false)),
                        column_name: Some(column_name),
                        data_type: DataType::Boolean,
                    });
                }
            }
            _ => *is_const = false,
        }
        *is_remove = false;
        Ok(new_expr)
    }

    fn try_convert_to_date(expr: Vec<Expression>) -> Result<Vec<Expression>> {
        let new_expr = expr
            .iter()
            .map(|e| match e {
                Expression::Literal {
                    value, data_type, ..
                } => {
                    if !matches!(data_type, DataType::String) {
                        return e.clone();
                    }
                    const DATE_FMT: &str = "%Y-%m-%d";
                    const TIME_FMT: &str = "%Y-%m-%d %H:%M:%S";
                    let date = NaiveDate::parse_from_str(value.to_string().as_str(), DATE_FMT);
                    let datetime =
                        NaiveDateTime::parse_from_str(value.to_string().as_str(), TIME_FMT);
                    let op_function;
                    if date.is_ok() {
                        op_function = "toDate";
                    } else if datetime.is_ok() {
                        op_function = "toDateTime";
                    } else {
                        return e.clone();
                    }
                    Self::rewrite_function(
                        op_function,
                        vec![e.clone()],
                        e.column_name(),
                        Expression::create_scalar_function,
                    )
                    .unwrap()
                }
                _ => e.clone(),
            })
            .collect::<Vec<Expression>>();
        Ok(new_expr)
    }
}

impl PlanRewriter for ConstantFoldingImpl {
    fn rewrite_expr(&mut self, schema: &DataSchemaRef, origin: &Expression) -> Result<Expression> {
        /* TODO: constant folding for subquery and scalar subquery
         * For example:
         *   before optimize: SELECT (SELECT 1 + 2)
         *   after optimize: SELECT 3
         */
        match origin {
            Expression::Alias(alias, expr) => {
                Self::rewrite_alias(alias, self.rewrite_expr(schema, expr)?)
            }
            Expression::ScalarFunction { op, args } => {
                let new_args = args
                    .iter()
                    .map(|expr| Self::rewrite_expr(self, schema, expr))
                    .collect::<Result<Vec<_>>>()?;

                let origin_name = origin.column_name();
                Self::rewrite_function(
                    op,
                    new_args,
                    origin_name,
                    Expression::create_scalar_function,
                )
            }
            Expression::UnaryExpression { op, expr } => {
                let origin_name = origin.column_name();
                let new_expr = vec![self.rewrite_expr(schema, expr)?];
                Self::rewrite_function(
                    op,
                    new_expr,
                    origin_name,
                    Expression::create_unary_expression,
                )
            }
            Expression::BinaryExpression { op, left, right } => match op.to_lowercase().as_str() {
                "and" => self.remove_const_cond(schema, origin.column_name(), left, right, true),
                "or" => self.remove_const_cond(schema, origin.column_name(), left, right, false),
                _ => {
                    let new_left = self.rewrite_expr(schema, left)?;
                    let new_right = self.rewrite_expr(schema, right)?;

                    let origin_name = origin.column_name();
                    let new_exprs = Self::try_convert_to_date(vec![new_left, new_right])?;
                    Self::rewrite_function(
                        op,
                        new_exprs,
                        origin_name,
                        Expression::create_binary_expression,
                    )
                }
            },
            Expression::Cast { expr, data_type } => {
                let new_expr = self.rewrite_expr(schema, expr)?;

                if matches!(&new_expr, Expression::Literal { .. }) {
                    let optimize_expr = Expression::Cast {
                        expr: Box::new(new_expr),
                        data_type: data_type.clone(),
                    };

                    return Self::execute_expression(optimize_expr, origin.column_name());
                }

                Ok(Expression::Cast {
                    expr: Box::new(new_expr),
                    data_type: data_type.clone(),
                })
            }
            Expression::Sort {
                expr,
                asc,
                nulls_first,
            } => {
                let new_expr = self.rewrite_expr(schema, expr)?;
                Ok(ConstantFoldingImpl::create_sort(asc, nulls_first, new_expr))
            }
            Expression::AggregateFunction {
                op,
                distinct,
                params,
                args,
            } => {
                let args = args
                    .iter()
                    .map(|expr| Self::rewrite_expr(self, schema, expr))
                    .collect::<Result<Vec<_>>>()?;

                let op = op.clone();
                let distinct = *distinct;
                let params = params.clone();
                Ok(Expression::AggregateFunction {
                    op,
                    distinct,
                    params,
                    args,
                })
            }
            _ => Ok(origin.clone()),
        }
    }

    fn rewrite_aggregate_partial(&mut self, plan: &AggregatorPartialPlan) -> Result<PlanNode> {
        let new_input = self.rewrite_plan_node(&plan.input)?;
        match self.before_group_by_schema {
            Some(_) => Err(ErrorCode::LogicalError(
                "Logical error: before group by schema must be None",
            )),
            None => {
                self.before_group_by_schema = Some(new_input.schema());
                let new_aggr_expr = self.rewrite_exprs(&new_input.schema(), &plan.aggr_expr)?;
                let new_group_expr = self.rewrite_exprs(&new_input.schema(), &plan.group_expr)?;
                PlanBuilder::from(&new_input)
                    .aggregate_partial(&new_aggr_expr, &new_group_expr)?
                    .build()
            }
        }
    }

    fn rewrite_aggregate_final(&mut self, plan: &AggregatorFinalPlan) -> Result<PlanNode> {
        let new_input = self.rewrite_plan_node(&plan.input)?;

        match self.before_group_by_schema.take() {
            None => Err(ErrorCode::LogicalError(
                "Logical error: before group by schema must be Some",
            )),
            Some(schema_before_group_by) => {
                let new_aggr_expr = self.rewrite_exprs(&new_input.schema(), &plan.aggr_expr)?;
                let new_group_expr = self.rewrite_exprs(&new_input.schema(), &plan.group_expr)?;
                PlanBuilder::from(&new_input)
                    .aggregate_final(schema_before_group_by, &new_aggr_expr, &new_group_expr)?
                    .build()
            }
        }
    }
}

impl ConstantFoldingImpl {
    pub fn new() -> ConstantFoldingImpl {
        ConstantFoldingImpl {
            before_group_by_schema: None,
        }
    }
}

impl Optimizer for ConstantFoldingOptimizer {
    fn name(&self) -> &str {
        "ConstantFolding"
    }

    fn optimize(&mut self, plan: &PlanNode) -> Result<PlanNode> {
        let mut visitor = ConstantFoldingImpl::new();
        visitor.rewrite_plan_node(plan)
    }
}

impl ConstantFoldingOptimizer {
    pub fn create(_ctx: DatabendQueryContextRef) -> Self {
        ConstantFoldingOptimizer {}
    }
}

impl ConstantFoldingImpl {
    fn create_sort(asc: &bool, nulls_first: &bool, new_expr: Expression) -> Expression {
        Expression::Sort {
            expr: Box::new(new_expr),
            asc: *asc,
            nulls_first: *nulls_first,
        }
    }
}
