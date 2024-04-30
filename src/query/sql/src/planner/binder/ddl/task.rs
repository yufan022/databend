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

use std::str::FromStr;

use chrono_tz;
use cron;
use databend_common_ast::ast::AlterTaskOptions;
use databend_common_ast::ast::AlterTaskStmt;
use databend_common_ast::ast::CreateTaskStmt;
use databend_common_ast::ast::DescribeTaskStmt;
use databend_common_ast::ast::DropTaskStmt;
use databend_common_ast::ast::ExecuteTaskStmt;
use databend_common_ast::ast::ScheduleOptions;
use databend_common_ast::ast::ShowTasksStmt;
use databend_common_ast::parser::parse_sql;
use databend_common_ast::parser::tokenize_sql;
use databend_common_ast::Dialect;
use databend_common_exception::ErrorCode;
use databend_common_exception::Result;

use crate::plans::AlterTaskPlan;
use crate::plans::CreateTaskPlan;
use crate::plans::DescribeTaskPlan;
use crate::plans::DropTaskPlan;
use crate::plans::ExecuteTaskPlan;
use crate::plans::Plan;
use crate::plans::ShowTasksPlan;
use crate::Binder;

fn verify_task_sql(sql: &String) -> Result<()> {
    let tokens = tokenize_sql(sql.as_str()).map_err(|e| {
        ErrorCode::SyntaxException(format!(
            "syntax error for task formatted sql: {}, error: {:?}",
            sql, e
        ))
    })?;
    parse_sql(&tokens, Dialect::PostgreSQL).map_err(|e| {
        ErrorCode::SyntaxException(format!(
            "syntax error for task formatted sql: {}, error: {:?}",
            sql, e
        ))
    })?;
    Ok(())
}

fn verify_scheduler_option(schedule_opts: &Option<ScheduleOptions>) -> Result<()> {
    if schedule_opts.is_none() {
        return Ok(());
    }
    let schedule_opts = schedule_opts.clone().unwrap();
    if let ScheduleOptions::CronExpression(cron_expr, time_zone) = schedule_opts {
        if cron::Schedule::from_str(&cron_expr).is_err() {
            return Err(ErrorCode::SemanticError(format!(
                "invalid cron expression {}",
                cron_expr
            )));
        }
        if let Some(time_zone) = time_zone && !time_zone.is_empty() && chrono_tz::Tz::from_str(&time_zone).is_err() {
            return Err(ErrorCode::SemanticError(format!(
                "invalid time zone {}",
                time_zone
            )));
        }
    }
    Ok(())
}

impl Binder {
    #[async_backtrace::framed]
    pub(in crate::planner::binder) async fn bind_create_task(
        &mut self,
        stmt: &CreateTaskStmt,
    ) -> Result<Plan> {
        let CreateTaskStmt {
            if_not_exists,
            name,
            warehouse_opts,
            schedule_opts,
            suspend_task_after_num_failures,
            comments,
            after,
            when_condition,
            sql,
            session_parameters,
        } = stmt;
        if (schedule_opts.is_none() && after.is_empty())
            || (schedule_opts.is_some() && !after.is_empty())
        {
            return Err(ErrorCode::SyntaxException(
                "task must be defined with either given time schedule as a root task or run after other task as a DAG".to_string(),
            ));
        }
        verify_scheduler_option(schedule_opts)?;
        verify_task_sql(sql)?;
        let tenant = self.ctx.get_tenant();
        let plan = CreateTaskPlan {
            if_not_exists: *if_not_exists,
            tenant,
            task_name: name.to_string(),
            warehouse_opts: warehouse_opts.clone(),
            schedule_opts: schedule_opts.clone(),
            suspend_task_after_num_failures: *suspend_task_after_num_failures,
            after: after.clone(),
            when_condition: when_condition.clone(),
            comment: comments.clone(),
            session_parameters: session_parameters.clone(),
            sql: sql.clone(),
        };
        Ok(Plan::CreateTask(Box::new(plan)))
    }

    #[async_backtrace::framed]
    pub(in crate::planner::binder) async fn bind_alter_task(
        &mut self,
        stmt: &AlterTaskStmt,
    ) -> Result<Plan> {
        let AlterTaskStmt {
            if_exists,
            name,
            options,
        } = stmt;

        if let AlterTaskOptions::Set {
            warehouse,
            schedule,
            suspend_task_after_num_failures,
            comments,
            session_parameters,
        } = options
        {
            if warehouse.is_none()
                && schedule.is_none()
                && suspend_task_after_num_failures.is_none()
                && comments.is_none()
                && session_parameters.is_none()
            {
                return Err(ErrorCode::SyntaxException(
                    "alter task must set at least one option".to_string(),
                ));
            }
            if schedule.is_some() {
                verify_scheduler_option(schedule)?;
            }
        }

        if let AlterTaskOptions::ModifyAs(sql) = options {
            verify_task_sql(sql)?;
        }

        let tenant = self.ctx.get_tenant();
        let plan = AlterTaskPlan {
            if_exists: *if_exists,
            tenant,
            task_name: name.to_string(),
            alter_options: options.clone(),
        };
        Ok(Plan::AlterTask(Box::new(plan)))
    }

    #[async_backtrace::framed]
    pub(in crate::planner::binder) async fn bind_drop_task(
        &mut self,
        stmt: &DropTaskStmt,
    ) -> Result<Plan> {
        let DropTaskStmt { if_exists, name } = stmt;

        let tenant = self.ctx.get_tenant();

        let plan = DropTaskPlan {
            if_exists: *if_exists,
            tenant,
            task_name: name.to_string(),
        };
        Ok(Plan::DropTask(Box::new(plan)))
    }

    #[async_backtrace::framed]
    pub(in crate::planner::binder) async fn bind_describe_task(
        &mut self,
        stmt: &DescribeTaskStmt,
    ) -> Result<Plan> {
        let DescribeTaskStmt { name } = stmt;

        let tenant = self.ctx.get_tenant();

        let plan = DescribeTaskPlan {
            tenant,
            task_name: name.to_string(),
        };
        Ok(Plan::DescribeTask(Box::new(plan)))
    }

    #[async_backtrace::framed]
    pub(in crate::planner::binder) async fn bind_execute_task(
        &mut self,
        stmt: &ExecuteTaskStmt,
    ) -> Result<Plan> {
        let ExecuteTaskStmt { name } = stmt;

        let tenant = self.ctx.get_tenant();

        let plan = ExecuteTaskPlan {
            tenant,
            task_name: name.to_string(),
        };
        Ok(Plan::ExecuteTask(Box::new(plan)))
    }

    #[async_backtrace::framed]
    pub(in crate::planner::binder) async fn bind_show_tasks(
        &mut self,
        stmt: &ShowTasksStmt,
    ) -> Result<Plan> {
        let ShowTasksStmt { limit } = stmt;

        let tenant = self.ctx.get_tenant();

        let plan = ShowTasksPlan {
            tenant,
            limit: limit.clone(),
        };
        Ok(Plan::ShowTasks(Box::new(plan)))
    }
}
