use super::*;

/// Every live agent as a rule sees it: the registry row, plus the
/// workspace/work stamped on its pane when a pane scan can supply them.
/// The daemon's automation task builds subjects the same way, so
/// `muxa automation test` and a real firing evaluate identical inputs.
pub(super) async fn automation_judge_test(
    rule: &AutomationRule,
    pane: &str,
    store: &SharedStore,
    backends: &[SharedBackend],
    automation: &AutomationStore,
    ask: &AskStore,
) -> Result<crate::automation_judge::AutomationJudgment, String> {
    use crate::automation::{AutomationLedgerEntry, AutomationOutcome};
    use crate::automation_judge::{capture_context, evaluate, AutomationJudgment};
    rule.validate()?;
    let condition = rule
        .ask_condition
        .as_ref()
        .ok_or("This rule has no Ask condition")?;
    let _permit = automation.try_judgment_slot()?;
    let agents = store.by_pane(pane).await;
    unique_pane_endpoint(pane, &agents)?;
    if agents.iter().any(|agent| agent.tmux_socket.is_none())
        && agents.iter().any(|agent| agent.tmux_socket.is_some())
    {
        return Err(
            "Ambiguous pane endpoint: both scoped and unscoped agents are registered".into(),
        );
    }
    if agents.len() > 1 {
        return Err("Ambiguous pane: multiple agent sessions are registered".into());
    }
    let agent = agents.first().ok_or("No live agent found for this pane")?;
    let config = automation.config().await;
    let saved = config.rule_named(&rule.name);
    let max_per_hour = condition.max_per_hour.min(6).min(
        saved
            .and_then(|rule| rule.ask_condition.as_ref())
            .map_or(6, |condition| condition.max_per_hour),
    );
    let cooldown = saved.map_or(time::Duration::seconds(10), |rule| {
        rule.cooldown().max(time::Duration::seconds(10))
    });
    let ledger = automation.ledger();
    let reservation = AutomationLedgerEntry {
        rule: rule.name.clone(),
        pane: pane.into(),
        agent: agent.kind,
        fired_at: time::OffsetDateTime::now_utc(),
        action: rule.action,
        outcome: AutomationOutcome::JudgeTest,
        detail: Some("Explicit Ask condition test; no action will be executed".into()),
        episode: None,
        operation: None,
        job: None,
    };
    ledger
        .reserve_judgment(reservation.clone(), max_per_hour, cooldown)
        .await?;
    let context = async {
        let subjects =
            tokio::time::timeout(Duration::from_secs(2), automation_subjects(store, backends))
                .await
                .map_err(|_| "Pane metadata capture timed out".to_string())?;
        let subject = subjects
            .iter()
            .find(|subject| {
                subject.agent_session_id == agent.session_id
                    && subject.socket == agent.tmux_socket
                    && subject.pane.as_deref() == Some(pane)
            })
            .ok_or("Agent changed before judgment capture".to_string())?;
        capture_context(subject, backends).await
    }
    .await;
    let judgment = match context {
        Ok(context) => evaluate(ask, condition, &context).await,
        Err(reason) => AutomationJudgment::unknown(condition, reason),
    };
    let detail = serde_json::json!({
        "judgment": judgment,
        "condition": condition.prompt,
        "observe_only": true,
        "execution": "test_only",
    })
    .to_string();
    ledger
        .append(AutomationLedgerEntry {
            fired_at: time::OffsetDateTime::now_utc(),
            outcome: AutomationOutcome::Judged,
            detail: Some(detail),
            ..reservation
        })
        .await;
    Ok(judgment)
}

pub(super) async fn automation_subjects(
    store: &SharedStore,
    backends: &[SharedBackend],
) -> Vec<AutomationSubject> {
    let agents = store.snapshot().await;
    let listed = backends.to_vec();
    let panes = tokio::task::spawn_blocking(move || {
        listed
            .iter()
            .flat_map(|backend| backend.list_panes())
            .collect::<Vec<_>>()
    })
    .await
    .unwrap_or_default();
    crate::automation::subjects_from(&agents, &panes)
}
