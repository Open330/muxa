//! Automation rules and their log.

#![allow(clippy::wildcard_imports)] // the arms name the IPC module's items as `dispatch.rs` does

use super::*;

pub(super) async fn handle(
    ctx: &ServerCtx,
    _conn: &mut Conn,
    turn: &mut Turn,
    body: RequestBody,
) -> Result<Flow, RuntimeError> {
    let store = &ctx.store;
    let backends = &ctx.backends;
    let ask = &ctx.ask;
    let automation = &ctx.automation;
    let kind = &mut turn.kind;
    let response = match body {
        RequestBody::AutomationList {} => {
            *kind = "automation_list";
            Response::with_automation_rules(automation.views(time::OffsetDateTime::now_utc()).await)
        }
        RequestBody::AutomationLog { limit } => {
            *kind = "automation_log";
            Response::with_automation_log(
                automation
                    .ledger()
                    .recent(limit.unwrap_or(crate::automation::MAX_LEDGER_ENTRIES))
                    .await,
            )
        }
        RequestBody::AutomationSetEnabled { name, enabled } => {
            *kind = "automation_set_enabled";
            // No name is the master switch: the whole engine, rather
            // than one rule.
            let applied = match name.as_deref() {
                Some(name) => automation.set_rule_enabled(name, enabled).await,
                None => automation.set_master_enabled(enabled).await,
            };
            match applied {
                Ok(()) => Response::with_automation_rules(
                    automation.views(time::OffsetDateTime::now_utc()).await,
                ),
                Err(error) => Response::err(error),
            }
        }
        RequestBody::AutomationPause { until } => {
            *kind = "automation_pause";
            match automation.set_paused_until(until).await {
                Ok(()) => Response::with_automation_rules(
                    automation.views(time::OffsetDateTime::now_utc()).await,
                ),
                Err(error) => Response::err(error),
            }
        }
        RequestBody::AutomationSetRule { rule } => {
            *kind = "automation_set_rule";
            match automation.upsert_rule(rule).await {
                Ok(()) => Response::with_automation_rules(
                    automation.views(time::OffsetDateTime::now_utc()).await,
                ),
                Err(error) => Response::err(error),
            }
        }
        RequestBody::AutomationRemoveRule { name } => {
            *kind = "automation_remove_rule";
            match automation.remove_rule(&name).await {
                Ok(()) => Response::with_automation_rules(
                    automation.views(time::OffsetDateTime::now_utc()).await,
                ),
                Err(error) => Response::err(error),
            }
        }
        RequestBody::AutomationTest { name } => {
            *kind = "automation_test";
            let subjects = automation_subjects(store, backends).await;
            match automation
                .test_rule(&name, &subjects, time::OffsetDateTime::now_utc())
                .await
            {
                Ok(report) => Response::with_automation_test(report),
                Err(error) => Response::err(error),
            }
        }
        RequestBody::AutomationJudgeTest { rule, pane } => {
            *kind = "automation_judge_test";
            match automation_judge_test(&rule, &pane, store, backends, automation, ask).await {
                Ok(judgment) => {
                    let mut response = Response::ok();
                    response.automation_judgment = Some(judgment);
                    response
                }
                Err(error) => Response::err(error),
            }
        }
        other => unreachable!("dispatch routed {other:?} to automation_ops"),
    };
    Ok(Flow::Respond(response))
}
