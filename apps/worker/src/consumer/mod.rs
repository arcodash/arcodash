//! Routes a dequeued `JobMessage` to its handler. The BLPOP loop lives in `valkey_consumer`.

pub mod valkey_consumer;

use arcodash_core::query_exec::ExecuteQueryPayload;
use arcodash_core::queue::JobMessage;

use crate::handlers::describe_data_source;
use crate::handlers::evaluate_monitor;
use crate::handlers::execute_query::{self, ExecuteQueryContext};
use crate::handlers::profile_data_source;
use crate::handlers::resume_monitor;
use crate::handlers::send_digest;
use crate::handlers::send_mail;
use crate::handlers::send_notification;

pub async fn dispatch(ctx: &ExecuteQueryContext, message: JobMessage) {
    match message {
        JobMessage::ExecuteQuery {
            job_id,
            org_id,
            data_source_id,
            query_text,
            parameters,
            user_id,
            query_id,
            scheduled,
            run_id,
            repoint_published,
            max_rows,
        } => {
            execute_query::handle(
                ctx,
                ExecuteQueryPayload {
                    job_id,
                    org_id,
                    data_source_id,
                    query_text,
                    parameters,
                    user_id,
                    query_id,
                    scheduled,
                    run_id,
                    repoint_published,
                    max_rows,
                },
            )
            .await;
        }
        JobMessage::EvaluateMonitor {
            org_id,
            monitor_id,
            run_id,
        } => {
            evaluate_monitor::handle(ctx, org_id, monitor_id, run_id).await;
        }
        JobMessage::SendNotification {
            org_id,
            monitor_id,
            destination_id,
            new_state,
            subscriber_id,
            subscription_id,
            attempt,
            run_id,
            observed,
            explanation,
        } => {
            send_notification::handle(
                ctx,
                org_id,
                monitor_id,
                destination_id,
                new_state,
                subscriber_id,
                subscription_id,
                attempt,
                run_id,
                observed,
                explanation,
            )
            .await;
        }
        JobMessage::ResumeMonitorEvaluation {
            org_id,
            run_id,
            query_run_id,
            query_run_status,
        } => {
            resume_monitor::handle(ctx, org_id, run_id, query_run_id, query_run_status).await;
        }
        JobMessage::SendDigest {
            org_id,
            user_id,
            delivery_id,
        } => {
            send_digest::handle(ctx, org_id, user_id, delivery_id).await;
        }
        JobMessage::SendMail {
            org_id,
            to,
            subject,
            body_html,
            delivery_id,
        } => {
            send_mail::handle(ctx, org_id, to, subject, body_html, delivery_id).await;
        }
        JobMessage::ProfileDataSource {
            org_id,
            data_source_id,
            data,
        } => {
            profile_data_source::handle(ctx, org_id, data_source_id, data).await;
        }
        JobMessage::DescribeDataSource {
            org_id,
            data_source_id,
        } => {
            describe_data_source::handle(ctx, org_id, data_source_id).await;
        }
    }
}
