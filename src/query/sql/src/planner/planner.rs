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

use std::sync::Arc;

use databend_common_ast::ast::Expr;
use databend_common_ast::ast::Literal;
use databend_common_ast::ast::Statement;
use databend_common_ast::parser::parse_sql;
use databend_common_ast::parser::token::Token;
use databend_common_ast::parser::token::TokenKind;
use databend_common_ast::parser::token::Tokenizer;
use databend_common_ast::walk_statement_mut;
use databend_common_ast::Dialect;
use databend_common_catalog::catalog::CatalogManager;
use databend_common_catalog::query_kind::QueryKind;
use databend_common_catalog::table_context::TableContext;
use databend_common_exception::Result;
use parking_lot::RwLock;

use super::semantic::AggregateRewriter;
use super::semantic::DistinctToGroupBy;
use crate::optimizer::optimize;
use crate::optimizer::OptimizerContext;
use crate::plans::Insert;
use crate::plans::InsertInputSource;
use crate::plans::Plan;
use crate::Binder;
use crate::Metadata;
use crate::MetadataRef;
use crate::NameResolutionContext;

const PROBE_INSERT_INITIAL_TOKENS: usize = 128;
const PROBE_INSERT_MAX_TOKENS: usize = 128 * 8;

pub struct Planner {
    ctx: Arc<dyn TableContext>,
}

#[derive(Debug, Clone)]
pub struct PlanExtras {
    pub metadata: MetadataRef,
    pub format: Option<String>,
    pub statement: Statement,
}

impl Planner {
    pub fn new(ctx: Arc<dyn TableContext>) -> Self {
        Planner { ctx }
    }

    #[async_backtrace::framed]
    #[minitrace::trace]
    pub async fn plan_sql(&mut self, sql: &str) -> Result<(Plan, PlanExtras)> {
        let settings = self.ctx.get_settings();
        let sql_dialect = settings.get_sql_dialect()?;
        // Step 1: Tokenize the SQL.
        let mut tokenizer = Tokenizer::new(sql).peekable();

        // Only tokenize the beginning tokens for `INSERT INTO` statement because the tokens of values is unused.
        //
        // Stop the tokenizer on unrecognized token because some values inputs (e.g. CSV) may not be valid for the tokenizer.
        // See also: https://github.com/datafuselabs/databend/issues/6669
        let is_insert_stmt = tokenizer
            .peek()
            .and_then(|token| Some(token.as_ref().ok()?.kind))
            == Some(TokenKind::INSERT);
        let mut tokens: Vec<Token> = if is_insert_stmt {
            (&mut tokenizer)
                .take(PROBE_INSERT_INITIAL_TOKENS)
                .take_while(|token| token.is_ok())
                // Make sure the tokens stream is always ended with EOI.
                .chain(std::iter::once(Ok(Token::new_eoi(sql))))
                .collect::<Result<_>>()
                .unwrap()
        } else {
            (&mut tokenizer).collect::<Result<_>>()?
        };

        loop {
            let res = async {
                // Step 2: Parse the SQL.
                let (mut stmt, format) = parse_sql(&tokens, sql_dialect)?;

                self.replace_stmt(&mut stmt, sql_dialect);

                // Step 3: Bind AST with catalog, and generate a pure logical SExpr
                let metadata = Arc::new(RwLock::new(Metadata::default()));
                let name_resolution_ctx = NameResolutionContext::try_from(settings.as_ref())?;
                let binder = Binder::new(
                    self.ctx.clone(),
                    CatalogManager::instance(),
                    name_resolution_ctx,
                    metadata.clone(),
                );

                // Indicate binder there is no need to collect column statistics for the binding table.
                self.ctx
                    .attach_query_str(get_query_kind(&stmt), stmt.to_mask_sql());
                let plan = binder.bind(&stmt).await?;
                // attach again to avoid the query kind is overwritten by the subquery
                self.ctx
                    .attach_query_str(get_query_kind(&stmt), stmt.to_mask_sql());

                // Step 4: Optimize the SExpr with optimizers, and generate optimized physical SExpr
                let opt_ctx = OptimizerContext::new(self.ctx.clone(), metadata.clone())
                    .with_enable_distributed_optimization(!self.ctx.get_cluster().is_empty())
                    .with_enable_join_reorder(unsafe {
                        !self.ctx.get_settings().get_disable_join_reorder()?
                    })
                    .with_enable_dphyp(self.ctx.get_settings().get_enable_dphyp()?);

                let optimized_plan = optimize(opt_ctx, plan)?;
                Ok((optimized_plan, PlanExtras {
                    metadata,
                    format,
                    statement: stmt,
                }))
            }
            .await;

            let mut maybe_partial_insert = false;
            if is_insert_stmt && matches!(tokenizer.peek(), Some(Ok(_))) {
                if let Ok((
                    Plan::Insert(box Insert {
                        source: InsertInputSource::SelectPlan(_),
                        ..
                    }),
                    _,
                )) = &res
                {
                    maybe_partial_insert = true;
                }
            }

            if maybe_partial_insert || (res.is_err() && matches!(tokenizer.peek(), Some(Ok(_)))) {
                // Remove the previous EOI.
                tokens.pop();
                // Tokenize more and try again.
                if tokens.len() < PROBE_INSERT_MAX_TOKENS {
                    let iter = (&mut tokenizer)
                        .take(tokens.len() * 2)
                        .take_while(|token| token.is_ok())
                        .map(|token| token.unwrap())
                        // Make sure the tokens stream is always ended with EOI.
                        .chain(std::iter::once(Token::new_eoi(sql)));
                    tokens.extend(iter);
                } else {
                    let iter = (&mut tokenizer)
                        .take_while(|token| token.is_ok())
                        .map(|token| token.unwrap())
                        // Make sure the tokens stream is always ended with EOI.
                        .chain(std::iter::once(Token::new_eoi(sql)));
                    tokens.extend(iter);
                };
            } else {
                return res;
            }
        }
    }

    fn add_max_rows_limit(&self, statement: &mut Statement) {
        let max_rows = self.ctx.get_settings().get_max_result_rows().unwrap();
        if max_rows == 0 {
            return;
        }

        if let Statement::Query(query) = statement {
            if query.limit.is_empty() {
                query.limit = vec![Expr::Literal {
                    span: None,
                    lit: Literal::UInt64(max_rows),
                }];
            }
        }
    }

    fn replace_stmt(&self, stmt: &mut Statement, sql_dialect: Dialect) {
        walk_statement_mut(&mut DistinctToGroupBy::default(), stmt);
        walk_statement_mut(&mut AggregateRewriter { sql_dialect }, stmt);

        self.add_max_rows_limit(stmt);
    }
}

pub fn get_query_kind(stmt: &Statement) -> QueryKind {
    match stmt {
        Statement::Query { .. } => QueryKind::Query,
        Statement::CopyIntoTable(_) => QueryKind::CopyIntoTable,
        Statement::CopyIntoLocation(_) => QueryKind::CopyIntoLocation,
        Statement::Explain { .. } => QueryKind::Explain,
        Statement::Insert(_) => QueryKind::Insert,
        Statement::Replace(_)
        | Statement::Delete(_)
        | Statement::MergeInto(_)
        | Statement::OptimizeTable(_)
        | Statement::Update(_) => QueryKind::Update,
        _ => QueryKind::Other,
    }
}
