use super::*;
use crate::test_support::*;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use tend::backend::{BackendCaps, HostKind, PaneBackend};
use tend::event::{AgentEvent, AgentId, AgentKind};
use tend::tmux::PaneInfo;
use time::OffsetDateTime;

fn resume_rule_config() -> tend::automation::AutomationConfig {
    let mut rule = AutomationRule::new(
        "resume-after-limit",
        tend::automation::AutomationEvent::RateLimited,
        AutomationAction::SendPrompt,
    );
    rule.text = Some("continue".into());
    rule.wait = Some(tend::automation::parse_wait("reset").unwrap());
    rule.jitter = Some(tend::automation::DurationSpec::seconds(0));
    tend::automation::AutomationConfig {
        rule: vec![rule],
        ..tend::automation::AutomationConfig::default()
    }
}

/// Every `(pane, text)` an automation typed, in order.
type RecordedSends = Arc<Mutex<Vec<(String, String)>>>;

fn automation_backend(panes: Vec<PaneInfo>) -> (Vec<tend::SharedBackend>, RecordedSends) {
    let sends = Arc::new(Mutex::new(Vec::new()));
    let backend: tend::SharedBackend = Arc::new(CollaborationWakeBackend {
        panes,
        sends: sends.clone(),
    });
    (vec![backend], sends)
}

struct JudgmentBackend {
    inner: CollaborationWakeBackend,
    screen: Arc<Mutex<String>>,
}

impl PaneBackend for JudgmentBackend {
    fn kind(&self) -> HostKind {
        HostKind::Tmux
    }
    fn list_panes(&self) -> Vec<PaneInfo> {
        self.inner.list_panes()
    }
    fn resolve_pane(&self, pane: &str) -> Option<PaneInfo> {
        self.inner.resolve_pane(pane)
    }
    fn capture_pane(&self, _: &str) -> Option<String> {
        Some(self.screen.lock().unwrap().clone())
    }
    fn pane_pid_map(&self) -> HashMap<u32, String> {
        HashMap::new()
    }
    fn current_pane(&self) -> Option<String> {
        None
    }
    fn focus_pane(&self, _: &str) -> bool {
        false
    }
    fn send_text(&self, pane: &str, text: &str) -> bool {
        self.inner.send_text(pane, text)
    }
    fn caps(&self) -> BackendCaps {
        BackendCaps::default()
    }
}

async fn judgment_fixture(
    observe_only: bool,
) -> (
    AutomationEngine,
    JudgedFiring,
    RecordedSends,
    Arc<Mutex<String>>,
) {
    let store = tend::Store::shared();
    add_capped_agent(&store, "%1", "capped").await;
    let sends = Arc::new(Mutex::new(Vec::new()));
    let screen = Arc::new(Mutex::new("Continue the agreed task?".to_string()));
    let mut pane = collaboration_pane("%1", "0");
    pane.workspace_id = Some("workspace".into());
    pane.work_id = Some("work".into());
    let backend: tend::SharedBackend = Arc::new(JudgmentBackend {
        inner: CollaborationWakeBackend {
            panes: vec![pane],
            sends: sends.clone(),
        },
        screen: screen.clone(),
    });
    let mut config = resume_rule_config();
    config.rule[0].ask_condition = Some(serde_json::from_value(serde_json::json!({
        "prompt": "Only continue the agreed task", "provider": "openai", "observe_only": observe_only,
    })).unwrap());
    let automation = AutomationStore::in_memory(config);
    let mut engine = AutomationEngine::new(automation, store, vec![backend]);
    engine.rescan().await;
    engine.fire_due().await;
    assert!(sends.lock().unwrap().is_empty());
    let mut completed = engine.judgments.join_next().await.unwrap().unwrap();
    assert_eq!(
        completed.context.as_ref().unwrap().work.as_deref(),
        Some("work")
    );
    assert_eq!(
        completed.context.as_ref().unwrap().workspace.as_deref(),
        Some("workspace")
    );
    completed.judgment.decision = JudgmentDecision::Match;
    completed.judgment.reason = "Test judgment".into();
    (engine, completed, sends, screen)
}

#[tokio::test]
async fn automation_queued_rule_change_is_refused_before_dispatch() {
    let store = tend::Store::shared();
    add_capped_agent(&store, "%1", "capped").await;
    let (backends, sends) = automation_backend(vec![collaboration_pane("%1", "0")]);
    let automation = AutomationStore::in_memory(resume_rule_config());
    let mut engine = AutomationEngine::new(automation.clone(), store, backends);
    engine.rescan().await;
    let mut edited = resume_rule_config().rule.remove(0);
    edited.text = Some("different action".into());
    automation.upsert_rule(edited).await.unwrap();
    engine.fire_due().await;
    assert!(sends.lock().unwrap().is_empty());
    assert!(engine.judgments.is_empty());
    assert!(automation.ledger().recent(1).await[0]
        .detail
        .as_deref()
        .unwrap()
        .contains("Queued rule"));
}

#[tokio::test]
async fn automation_judgment_match_sends_only_the_fixed_action_once() {
    let (mut engine, completed, sends, _) = judgment_fixture(false).await;
    engine.finish_judgment(completed).await;
    assert_eq!(
        *sends.lock().unwrap(),
        vec![("%1".into(), "continue".into()), ("%1".into(), "\r".into())]
    );
    engine.rescan().await;
    engine.fire_due().await;
    assert_eq!(sends.lock().unwrap().len(), 2);
    assert!(engine.judgments.is_empty());
}

#[tokio::test]
async fn automation_judgment_observation_never_sends() {
    let (mut engine, completed, sends, _) = judgment_fixture(true).await;
    engine.finish_judgment(completed).await;
    assert!(sends.lock().unwrap().is_empty());
    assert!(engine.automation.ledger().recent(1).await[0]
        .detail
        .as_ref()
        .unwrap()
        .contains("observe_only"));
    engine.rescan().await;
    assert!(engine.pending.is_empty());
}

#[tokio::test]
async fn automation_judgment_changed_output_or_state_discards_match() {
    let (mut engine, completed, sends, screen) = judgment_fixture(false).await;
    *screen.lock().unwrap() = "Deploy to production?".into();
    engine.finish_judgment(completed).await;
    assert!(sends.lock().unwrap().is_empty());
    assert!(engine.automation.ledger().recent(1).await[0]
        .detail
        .as_ref()
        .unwrap()
        .contains("context_changed"));
    let (mut engine, completed, sends, _) = judgment_fixture(false).await;
    add_agent(&engine.store, "%1", "capped", AgentKind::ClaudeCode).await;
    engine.finish_judgment(completed).await;
    assert!(sends.lock().unwrap().is_empty());
}

#[tokio::test]
async fn automation_judgment_config_edit_discards_match() {
    let (mut engine, completed, sends, _) = judgment_fixture(false).await;
    engine
        .automation
        .set_paused_until(Some(OffsetDateTime::now_utc() + time::Duration::minutes(5)))
        .await
        .unwrap();
    engine.finish_judgment(completed).await;
    assert!(sends.lock().unwrap().is_empty());
}

#[tokio::test]
async fn automation_judgment_unknown_and_no_match_never_send() {
    for decision in [JudgmentDecision::Unknown, JudgmentDecision::NoMatch] {
        let (mut engine, mut completed, sends, _) = judgment_fixture(false).await;
        completed.judgment.decision = decision;
        engine.finish_judgment(completed).await;
        assert!(sends.lock().unwrap().is_empty());
        engine.rescan().await;
        assert!(engine.pending.is_empty());
    }
}

#[tokio::test]
async fn automation_enter_sends_one_key_only_for_an_affirmative_choice() {
    for (screen, expected) in [
        ("› 1. Yes, continue\n  2. No\nPress enter to continue", true),
        ("  1. Yes\n› 2. No\nPress enter to continue", false),
        ("› Type your reply", false),
    ] {
        let store = tend::Store::shared();
        add_agent(&store, "%1", "waiting", AgentKind::ClaudeCode).await;
        store
            .apply(&AgentEvent::NotificationFired {
                id: AgentId {
                    kind: AgentKind::ClaudeCode,
                    session_id: "waiting".into(),
                    surface: None,
                    pane: Some("%1".into()),
                    tmux_socket: Some("default".into()),
                    cwd: Some("/repo".into()),
                },
                level: tend::event::NotificationLevel::NeedsChoice,
                message: "Confirm choice".into(),
                at: OffsetDateTime::now_utc(),
            })
            .await;
        let sends = Arc::new(Mutex::new(Vec::new()));
        let backend: tend::SharedBackend = Arc::new(JudgmentBackend {
            inner: CollaborationWakeBackend {
                panes: vec![collaboration_pane("%1", "0")],
                sends: sends.clone(),
            },
            screen: Arc::new(Mutex::new(screen.into())),
        });
        let mut rule = tend::automation_defaults::rules()
            .into_iter()
            .find(|rule| rule.name == "confirm-selected-yes")
            .unwrap();
        rule.enabled = true;
        rule.wait = Some(tend::automation::parse_wait("0s").unwrap());
        let config = tend::automation::AutomationConfig {
            rule: vec![rule],
            ..Default::default()
        };
        let automation = AutomationStore::in_memory(config);
        let mut engine = AutomationEngine::new(automation, store, vec![backend]);
        for _ in 0..2 {
            engine.rescan().await;
            engine.fire_due().await;
        }
        let expected_sends = if expected {
            vec![("%1".into(), "\r".into())]
        } else {
            vec![]
        };
        assert_eq!(*sends.lock().unwrap(), expected_sends, "{screen}");
    }
}

#[tokio::test]
async fn automation_resumes_a_capped_agent_by_typing_into_its_pane() {
    let store = tend::Store::shared();
    add_capped_agent(&store, "%1", "capped").await;
    let (backends, sends) = automation_backend(vec![collaboration_pane("%1", "0")]);
    let automation = AutomationStore::in_memory(resume_rule_config());
    let mut engine = AutomationEngine::new(automation.clone(), store, backends);

    engine.rescan().await;
    engine.fire_due().await;

    // The prompt, then the submit CR — the same two-step the
    // collaboration waker uses.
    assert_eq!(
        *sends.lock().unwrap(),
        vec![
            ("%1".to_string(), "continue".to_string()),
            ("%1".to_string(), "\r".to_string()),
        ],
    );
    let log = automation.ledger().all().await;
    assert_eq!(log.len(), 1);
    assert_eq!(log[0].outcome, AutomationOutcome::Fired);
    assert_eq!(log[0].rule, "resume-after-limit");
    assert_eq!(log[0].pane, "%1");
}

#[tokio::test]
async fn automation_fires_once_per_cap_episode() {
    let store = tend::Store::shared();
    add_capped_agent(&store, "%1", "capped").await;
    let (backends, sends) = automation_backend(vec![collaboration_pane("%1", "0")]);
    let automation = AutomationStore::in_memory(resume_rule_config());
    let mut engine = AutomationEngine::new(automation.clone(), store, backends);

    for _ in 0..3 {
        engine.rescan().await;
        engine.fire_due().await;
    }

    // The row is still capped and still in the same episode, so the
    // ledger's episode key stops every re-arm after the first.
    assert_eq!(sends.lock().unwrap().len(), 2, "one prompt, one submit");
    let log = automation.ledger().all().await;
    assert_eq!(log.len(), 1, "{log:?}");
}

#[tokio::test]
async fn automation_skips_an_agent_that_recovered_before_the_firing_was_due() {
    let store = tend::Store::shared();
    add_capped_agent(&store, "%1", "capped").await;
    let (backends, sends) = automation_backend(vec![collaboration_pane("%1", "0")]);
    let automation = AutomationStore::in_memory(resume_rule_config());
    let mut engine = AutomationEngine::new(automation.clone(), store.clone(), backends);
    engine.rescan().await;

    // The operator got there first: a fresh `Started` clears the cap.
    add_agent(&store, "%1", "capped", AgentKind::ClaudeCode).await;
    engine.fire_due().await;

    assert!(
        sends.lock().unwrap().is_empty(),
        "a recovered agent must not be typed into",
    );
    let log = automation.ledger().all().await;
    assert_eq!(log.len(), 1);
    assert_eq!(log[0].outcome, AutomationOutcome::Skipped);
    assert_eq!(
        log[0].detail.as_deref(),
        Some(SkipReason::ConditionCleared.to_string()).as_deref(),
    );
}

#[tokio::test]
async fn automation_with_the_engine_paused_does_nothing() {
    let store = tend::Store::shared();
    add_capped_agent(&store, "%1", "capped").await;
    let (backends, sends) = automation_backend(vec![collaboration_pane("%1", "0")]);
    let mut config = resume_rule_config();
    config.paused_until = Some(OffsetDateTime::now_utc() + time::Duration::hours(1));
    let automation = AutomationStore::in_memory(config);
    let mut engine = AutomationEngine::new(automation.clone(), store, backends);

    engine.rescan().await;
    engine.fire_due().await;

    assert!(sends.lock().unwrap().is_empty());
    assert_eq!(
        automation.ledger().all().await,
        [] as [tend::automation::AutomationLedgerEntry; 0]
    );
}

#[tokio::test]
async fn automation_with_no_rules_is_inert() {
    let store = tend::Store::shared();
    add_capped_agent(&store, "%1", "capped").await;
    let (backends, sends) = automation_backend(vec![collaboration_pane("%1", "0")]);
    // The shipped default: enabled, with no rules at all.
    let automation = AutomationStore::in_memory(tend::automation::AutomationConfig::default());
    let mut engine = AutomationEngine::new(automation.clone(), store, backends);

    engine.rescan().await;
    engine.fire_due().await;

    assert!(sends.lock().unwrap().is_empty());
    assert_eq!(
        automation.ledger().all().await,
        [] as [tend::automation::AutomationLedgerEntry; 0]
    );
}

#[tokio::test]
async fn automation_honours_the_workspace_and_work_filters() {
    let store = tend::Store::shared();
    add_capped_agent(&store, "%1", "match").await;
    add_capped_agent(&store, "%2", "miss").await;
    let mut matching = collaboration_pane("%1", "0");
    matching.workspace_id = Some("webapp".into());
    matching.work_id = Some("ENG-1234".into());
    let mut other = collaboration_pane("%2", "1");
    other.workspace_id = Some("webapp".into());
    other.work_id = Some("JIRA-9".into());
    let (backends, sends) = automation_backend(vec![matching, other]);

    let mut config = resume_rule_config();
    config.rule[0].workspace = Some("webapp".into());
    config.rule[0].work = Some("^ENG-".into());
    let automation = AutomationStore::in_memory(config);
    let mut engine = AutomationEngine::new(automation, store, backends);

    engine.rescan().await;
    engine.fire_due().await;

    let sends = sends.lock().unwrap();
    assert!(
        sends.iter().all(|(pane, _)| pane == "%1"),
        "only the pane whose work id matches may be typed into: {sends:?}",
    );
    assert_eq!(sends.len(), 2);
}

#[cfg(target_os = "linux")]
struct Child(std::process::Child);

#[cfg(target_os = "linux")]
impl Drop for Child {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

#[cfg(target_os = "linux")]
#[tokio::test]
#[allow(clippy::too_many_lines)] // three persisted drain gates share one worker fixture
async fn worker_drain_gates_ordinary_wakes_full_delivery_and_automation_io() {
    let child = Child(
        std::process::Command::new("bash")
            .args(["-c", "exec -a 'codex --no-daemon' sleep 60"])
            .spawn()
            .unwrap(),
    );
    for _ in 0..300 {
        if std::fs::read(format!("/proc/{}/cmdline", child.0.id()))
            .unwrap_or_default()
            .starts_with(b"codex --no-daemon")
        {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    let store = tend::Store::shared();
    add_agent(&store, "%2", "worker", AgentKind::Codex).await;
    let event=serde_json::from_value(serde_json::json!({"type":"turn_stopped","id":{"kind":"codex","session_id":"worker","pane":"%2","tmux_socket":"default"},"response":"ready","at":OffsetDateTime::now_utc().format(&time::format_description::well_known::Rfc3339).unwrap()})).unwrap();
    store.apply(&event).await;
    let mut pane = collaboration_pane("%2", "1");
    pane.pane_pid = child.0.id();
    pane.current_command = "codex".into();
    let (backends, sends) = automation_backend(vec![pane.clone()]);
    let rows = store.snapshot().await;
    let participants = worker_participants(&rows, std::slice::from_ref(&pane));
    let p = participants
        .iter()
        .find(|p| p.agent_session_id == "worker")
        .unwrap();
    let c = CollaborationStore::in_memory(CollaborationOptions::default());
    authorize_test_workers(&c, std::slice::from_ref(p)).await;
    let acquired = c
        .worker_drain_acquire(
            &tend::collaboration::WorkerDrainTarget::from(p),
            tend::collaboration::WorkerDrainPolicy {
                min_observed_idle_secs: 0,
                allow_error: false,
            },
            &participants,
            &rows,
            &[pane],
        )
        .await
        .unwrap();
    assert!(matches!(
        acquired,
        tend::collaboration::WorkerDrainAcquire::Acquired { .. }
    ));
    assert_eq!(
        send_collaboration_wake(&c, p, "notice", &backends, &store).await,
        (false, false)
    );
    let other = CollaborationStore::in_memory(CollaborationOptions::default());
    let mut sender = p.clone();
    sender.agent_session_id = "sender".into();
    sender.pane = "%1".into();
    let request = other
        .create(
            sender,
            p.clone(),
            NewRequest {
                body: "request".into(),
                ..NewRequest::default()
            },
        )
        .await
        .unwrap();
    let request = other
        .prepare_direct_wake(p, &request.id)
        .await
        .unwrap()
        .unwrap();
    assert!(
        !deliver_full_collaboration_request(
            &c,
            p,
            &request,
            &backends,
            0,
            CollaborationWakePayload::Full
        )
        .await
    );
    #[cfg(feature = "aas")]
    {
        use crate::swap_runner::DaemonContext;
        // A swap job takes the pane the same way, and is refused the same way.
        let context = DaemonContext {
            runtime: tokio::runtime::Handle::current(),
            store: store.clone(),
            backends: backends.clone(),
            swap: tend::swap_api::SwapConfig::default(),
            accounts: tend::accounts::AccountsConfig::default(),
            config_path: None,
            node_id: "node".into(),
            collaboration: Arc::clone(&c),
            receipts: Arc::new(tend_aas::swap_service::PromptReceipts::new(Arc::default())),
        };
        let (pane_id, socket) = (p.pane.clone(), p.socket.clone());
        let refused = tokio::task::spawn_blocking(move || {
            use tend_aas::swap_service::ContextProvider as _;
            context
                .acquire_guard(&pane_id, socket.as_deref())
                .map(|_| ())
        })
        .await
        .unwrap();
        assert!(refused.unwrap_err().contains("blocked"));
    }
    let mut engine = AutomationEngine::new(
        AutomationStore::in_memory(resume_rule_config()),
        store,
        backends,
    );
    engine.collaboration = Some(c);
    let firing = PlannedFiring {
        rule: "rule".into(),
        pane: p.pane.clone(),
        socket: p.socket.clone(),
        agent_session_id: p.agent_session_id.clone(),
        agent: p.agent_kind,
        episode: "episode".into(),
        fire_at: OffsetDateTime::now_utc(),
        action: AutomationAction::SendPrompt,
        text: Some("automation".into()),
        submit: true,
        only_if_still: tend::automation::AutomationCondition::Idle,
        account: None,
    };
    assert!(matches!(engine.perform(&firing).await, Performed::Failed));
    assert!(sends.lock().unwrap().is_empty());
}

#[cfg(feature = "aas")]
mod swap_tests {
    use super::*;
    use crate::swap_runner::*;

    /// The daemon's pane guard is per pane name: with no server it covers
    /// that pane on any, with one only that endpoint, and never another pane.
    /// The store's prompt hooks reach the swap probe, by conversation and
    /// pane, without their text.
    #[tokio::test]
    async fn prompt_hooks_reach_the_swap_probe() {
        use tend_aas::account_swap::TurnProbe as _;
        let store = tend::Store::shared();
        add_agent(&store, "%1", "conv", AgentKind::ClaudeCode).await;
        let receipts = Arc::new(tend_aas::swap_service::PromptReceipts::new(
            store.prompt_sequence(),
        ));
        let prompt = || -> tend::AgentEvent {
            serde_json::from_value(serde_json::json!({
                "type": "prompt_submitted",
                "id": {"kind": "claude_code", "session_id": "conv", "pane": "%1", "tmux_socket": "default"},
                "prompt": "continue",
                "at": OffsetDateTime::now_utc()
                    .format(&time::format_description::well_known::Rfc3339)
                    .unwrap(),
            }))
            .unwrap()
        };
        let endpoint = tend_aas::swap_service::endpoint("%1", Some("default"));
        // Forwarding held: an old hook is accepted, the cursor taken, and
        // only then is the old hook received. It is not after the cursor.
        let mut held = store.subscribe_prompts();
        store.apply(&prompt()).await;
        let cursor = receipts.cursor();
        receipts.record(&held.recv().await.unwrap());
        assert!(!receipts.prompt_after(cursor, AgentKind::ClaudeCode, "conv", &endpoint));
        // A new one, through the real forwarder, is.
        spawn_prompt_receipts(&store, Arc::clone(&receipts));
        tokio::task::yield_now().await;
        store.apply(&prompt()).await;
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while !receipts.prompt_after(cursor, AgentKind::ClaudeCode, "conv", &endpoint) {
            assert!(
                std::time::Instant::now() < deadline,
                "the hook never arrived"
            );
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        assert!(!receipts.prompt_after(cursor, AgentKind::ClaudeCode, "other", &endpoint));
    }

    /// The automation's still-due check follows the live store and the
    /// configuration: a new prompt (another episode) or an edited rule makes
    /// the firing stale.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn an_automation_swap_goes_stale_with_a_new_episode_or_config() {
        let store = tend::Store::shared();
        add_capped_agent(&store, "%1", "capped").await;
        let agent = store.by_session("capped").await.unwrap();
        let subject = AutomationSubject::from_agent(&agent);
        let automation = AutomationStore::in_memory(resume_rule_config());
        let firing = PlannedFiring {
            rule: "resume-after-limit".into(),
            pane: "%1".into(),
            socket: subject.socket.clone(),
            agent_session_id: "capped".into(),
            agent: AgentKind::ClaudeCode,
            episode: subject.episode(),
            fire_at: OffsetDateTime::now_utc(),
            action: AutomationAction::SwapAccount,
            text: None,
            submit: false,
            only_if_still: tend::automation::AutomationCondition::RateLimited,
            account: None,
        };
        let revision = *automation.subscribe().borrow();
        let check = |store: &tend::SharedStore| {
            automation_still_due(
                tokio::runtime::Handle::current(),
                store.clone(),
                automation.subscribe(),
                &firing,
                revision,
            )
        };
        let due = check(&store);
        assert!(tokio::task::spawn_blocking(due).await.unwrap().is_ok());
        // The rule edited: stale.
        let edited = check(&store);
        automation
            .set_rule_enabled("resume-after-limit", false)
            .await
            .unwrap();
        assert!(tokio::task::spawn_blocking(edited).await.unwrap().is_err());
        // A new prompt: another episode, stale even under the old revision.
        let (_unchanged, revision_now) = tokio::sync::watch::channel(revision);
        let stale = automation_still_due(
            tokio::runtime::Handle::current(),
            store.clone(),
            revision_now,
            &firing,
            revision,
        );
        add_agent(&store, "%1", "capped", AgentKind::ClaudeCode).await;
        assert!(tokio::task::spawn_blocking(stale).await.unwrap().is_err());
    }

    /// A configuration change that lands while the check reads the agent
    /// still makes the firing stale: the revision is read again last.
    #[tokio::test]
    async fn a_config_change_during_the_agent_read_is_seen() {
        let store = tend::Store::shared();
        add_capped_agent(&store, "%1", "capped").await;
        let agent = store.by_session("capped").await.unwrap();
        let subject = AutomationSubject::from_agent(&agent);
        let (bump, revision_now) = tokio::sync::watch::channel(0_u64);
        let firing = PlannedFiring {
            rule: "r".into(),
            pane: "%1".into(),
            socket: subject.socket.clone(),
            agent_session_id: "capped".into(),
            agent: AgentKind::ClaudeCode,
            episode: subject.episode(),
            fire_at: OffsetDateTime::now_utc(),
            action: AutomationAction::SwapAccount,
            text: None,
            submit: false,
            only_if_still: tend::automation::AutomationCondition::RateLimited,
            account: None,
        };
        let read_at = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let reads = Arc::clone(&read_at);
        let unchanged = still_due_from(
            {
                let agent = agent.clone();
                move |_| Some(agent.clone())
            },
            bump.subscribe(),
            &firing,
            0,
        );
        assert!(unchanged().is_ok());
        // The rule is edited exactly while the agent is being read.
        let check = still_due_from(
            move |_| {
                reads.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                bump.send(1).unwrap();
                Some(agent.clone())
            },
            revision_now,
            &firing,
            0,
        );
        assert_eq!(check().unwrap_err(), "the automation configuration changed");
        assert_eq!(read_at.load(std::sync::atomic::Ordering::SeqCst), 1);
    }

    #[test]
    fn a_pane_guard_covers_its_pane_and_no_other_server() {
        use tend_aas::swap_service::TargetGuard as _;
        let gate = || {
            Arc::new(
                Arc::new(tokio::sync::Mutex::new(()))
                    .try_lock_owned()
                    .unwrap(),
            )
        };
        let endpoint = tend_aas::swap_service::endpoint;
        let any = PaneGuard {
            pane: "%1".into(),
            socket: None,
            _gate: gate(),
        };
        assert!(any.covers(&endpoint("%1", Some("/tmp/tmux-1/default"))));
        assert!(!any.covers(&endpoint("%10", Some("/tmp/tmux-1/default"))));
        assert!(!any.covers(&endpoint("%2", Some("/tmp/tmux-1/default"))));
        let pinned = PaneGuard {
            pane: "%1".into(),
            socket: Some("default".into()),
            _gate: gate(),
        };
        assert!(pinned.covers(&endpoint("%1", Some("/tmp/tmux-1/default"))));
        assert!(!pinned.covers(&endpoint("%1", Some("/tmp/tmux-1/other"))));
    }

    /// Without swap jobs an automation swap is refused outright: nothing
    /// falls back to running a swap that is not journalled.
    #[tokio::test]
    async fn an_automation_swap_without_swap_jobs_is_refused() {
        let store = tend::Store::shared();
        let (backends, sends) = automation_backend(Vec::new());
        let mut engine = AutomationEngine::new(
            AutomationStore::in_memory(resume_rule_config()),
            store,
            backends,
        );
        let mut rule = resume_rule_config().rule[0].clone();
        rule.action = AutomationAction::SwapAccount;
        let firing = PlannedFiring {
            rule: rule.name.clone(),
            pane: "%1".into(),
            socket: Some("default".into()),
            agent_session_id: "sid".into(),
            agent: AgentKind::ClaudeCode,
            episode: "episode".into(),
            fire_at: OffsetDateTime::now_utc(),
            action: AutomationAction::SwapAccount,
            text: None,
            submit: false,
            only_if_still: tend::automation::AutomationCondition::Idle,
            account: None,
        };
        assert!(matches!(
            engine
                .admit_swap(&firing, &rule, 0, None, false, &mut None)
                .await,
            Admission::Unavailable
        ));
        assert!(engine.swap_inflight.is_empty());
        assert!(sends.lock().unwrap().is_empty());
    }

    type Counter = Arc<std::sync::atomic::AtomicUsize>;

    fn count(counter: &Counter) -> usize {
        counter.load(std::sync::atomic::Ordering::SeqCst)
    }

    /// The daemon's agents as they were when the fixture was made; counts
    /// what the service asks of it.
    struct FixtureContext {
        agents: Vec<tend::Agent>,
        snapshots: Counter,
        guards: Counter,
        /// Closed: planning waits, as a slow one does.
        planning: Arc<(Mutex<bool>, std::sync::Condvar)>,
    }

    impl tend_aas::swap_service::ContextProvider for FixtureContext {
        fn snapshot(&self) -> Result<tend_aas::swap_service::Snapshot, String> {
            self.snapshots
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            let (open, turned) = &*self.planning;
            let mut open = open.lock().unwrap();
            while !*open {
                open = turned.wait(open).unwrap();
            }
            drop(open);
            Ok(tend_aas::swap_service::Snapshot {
                node_id: "node".into(),
                agents: self.agents.clone(),
                backends: Vec::new(),
                config: tend::swap_api::SwapConfig::default(),
                accounts: tend::accounts::AccountsConfig::default(),
                config_path: None,
            })
        }

        fn acquire_guard(
            &self,
            _pane: &str,
            _socket: Option<&str>,
        ) -> Result<Box<dyn tend_aas::swap_service::TargetGuard>, String> {
            self.guards
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Err("an automation swap must bring its own guard".into())
        }
    }

    /// An automation engine with a swap rule over one capped agent, its
    /// swaps run by a real job service whose engine touches nothing and
    /// whose targets can be held mid-run.
    struct SwapFixture {
        engine: AutomationEngine,
        automation: Arc<AutomationStore>,
        collaboration: Arc<CollaborationStore>,
        service: Arc<tend_aas::swap_service::SwapService>,
        swap: Arc<DaemonSwap>,
        sends: RecordedSends,
        gate: Arc<(Mutex<bool>, std::sync::Condvar)>,
        planning: Arc<(Mutex<bool>, std::sync::Condvar)>,
        snapshots: Counter,
        guards: Counter,
        runs: Counter,
        key: String,
        _dir: tempfile::TempDir,
    }

    impl SwapFixture {
        async fn new() -> Self {
            let store = tend::Store::shared();
            add_capped_agent(&store, "%1", "capped").await;
            let agents = store.snapshot().await;
            let episode = AutomationSubject::from_agent(&agents[0]).episode();
            let (backends, sends) = automation_backend(vec![collaboration_pane("%1", "0")]);
            let mut rule = AutomationRule::new(
                "swap-on-limit",
                tend::automation::AutomationEvent::RateLimited,
                AutomationAction::SwapAccount,
            );
            rule.jitter = Some(tend::automation::DurationSpec::seconds(0));
            let automation = AutomationStore::in_memory(tend::automation::AutomationConfig {
                rule: vec![rule],
                ..tend::automation::AutomationConfig::default()
            });
            let collaboration = CollaborationStore::in_memory(CollaborationOptions::default());
            let gate = Arc::new((Mutex::new(true), std::sync::Condvar::new()));
            let planning = Arc::new((Mutex::new(true), std::sync::Condvar::new()));
            let (snapshots, guards, runs) =
                (Counter::default(), Counter::default(), Counter::default());
            let dir = tempfile::tempdir().unwrap();
            let (held, ran) = (Arc::clone(&gate), Arc::clone(&runs));
            let service = Arc::new(
                tend_aas::swap_service::testing::open_inert(
                    &dir.path().join("swap-jobs.sqlite3"),
                    Arc::new(FixtureContext {
                        agents,
                        snapshots: Arc::clone(&snapshots),
                        guards: Arc::clone(&guards),
                        planning: Arc::clone(&planning),
                    }),
                    Arc::new(move || {
                        ran.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                        let (open, turned) = &*held;
                        let mut open = open.lock().unwrap();
                        while !*open {
                            open = turned.wait(open).unwrap();
                        }
                    }),
                )
                .unwrap(),
            );
            let mut engine = AutomationEngine::new(automation.clone(), store, backends);
            engine.collaboration = Some(Arc::clone(&collaboration));
            let swap = Arc::new(DaemonSwap {
                service: Arc::clone(&service),
                node_id: "node".into(),
                submissions: Arc::default(),
            });
            engine.swap_service = Some(Arc::clone(&swap));
            let key = swap_runner::request_key(
                "node",
                "%1",
                Some("default"),
                "capped",
                "swap-on-limit",
                &episode,
            );
            Self {
                engine,
                automation,
                collaboration,
                service,
                swap,
                sends,
                gate,
                planning,
                snapshots,
                guards,
                runs,
                key,
                _dir: dir,
            }
        }

        fn hold(&self, held: bool) {
            let (open, turned) = &*self.gate;
            *open.lock().unwrap() = !held;
            turned.notify_all();
        }

        fn hold_planning(&self, held: bool) {
            let (open, turned) = &*self.planning;
            *open.lock().unwrap() = !held;
            turned.notify_all();
        }

        fn take_engine(&mut self) -> AutomationEngine {
            std::mem::replace(
                &mut self.engine,
                AutomationEngine::new(
                    AutomationStore::in_memory(tend::automation::AutomationConfig::default()),
                    tend::Store::shared(),
                    Vec::new(),
                ),
            )
        }

        fn job(&self) -> tend::swap_api::JobView {
            use tend::swap_api::SwapJobs as _;
            self.service
                .status(&tend::swap_api::JobRef::Key(self.key.clone()))
                .unwrap()
        }

        async fn pane_is_free(&self) -> bool {
            tokio::time::timeout(
                std::time::Duration::from_millis(500),
                self.collaboration.worker_action_guard("%1"),
            )
            .await
            .is_ok()
        }
    }

    /// A failed assertion still opens the gate, so the runtime's teardown
    /// never waits on a held job.
    impl Drop for SwapFixture {
        fn drop(&mut self) {
            self.hold(false);
            self.hold_planning(false);
        }
    }

    impl SwapFixture {
        /// Wait for the one operation's task and let the engine record it.
        async fn settle(&mut self) {
            let ended = tokio::time::timeout(
                std::time::Duration::from_secs(10),
                self.engine.swap_ops.join_next_with_id(),
            )
            .await
            .expect("the operation never ended")
            .expect("an operation was running");
            self.engine.swap_task_ended(ended).await;
        }

        async fn outcomes(&self) -> Vec<(AutomationOutcome, Option<String>)> {
            self.automation
                .ledger()
                .all()
                .await
                .into_iter()
                .map(|entry| (entry.outcome, entry.operation))
                .collect()
        }

        async fn wait_for_runs(&self, runs: usize) {
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
            while count(&self.runs) < runs {
                assert!(
                    std::time::Instant::now() < deadline,
                    "the swap never started running"
                );
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
        }
    }

    /// The engine admits the swap and goes on at once; the job holds the
    /// automation's own guard until it ends, and its end is recorded once.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn an_automation_swap_hands_its_pane_guard_to_the_job_until_it_ends() {
        let mut fixture = SwapFixture::new().await;
        fixture.hold(true);
        fixture.engine.rescan().await;
        tokio::time::timeout(std::time::Duration::from_secs(2), fixture.engine.fire_due())
            .await
            .expect("the engine must not wait on the swap");
        fixture.wait_for_runs(1).await;
        // An accepted, journalled job is running its one target.
        assert_eq!(fixture.job().state, "running");
        assert!(!fixture.pane_is_free().await);
        let key = Some(fixture.key.clone());
        assert_eq!(
            fixture.outcomes().await,
            vec![(AutomationOutcome::Admitted, key.clone())]
        );
        // Admitted is charged at once: the guards see it.
        let guards = fixture
            .automation
            .ledger()
            .guard_state("swap-on-limit", "%1", "", OffsetDateTime::now_utc())
            .await;
        assert_eq!(guards.global_fired_last_hour, 1);
        fixture.hold(false);
        fixture.settle().await;
        assert!(fixture.pane_is_free().await);
        let job = fixture.job();
        assert_eq!(job.state, "done");
        assert_eq!(job.entries[0].disposition.as_deref(), Some("swapped"));
        assert_eq!(count(&fixture.runs), 1);
        assert_eq!(
            count(&fixture.guards),
            0,
            "the job never takes the pane again"
        );
        let log = fixture.automation.ledger().all().await;
        assert_eq!(
            fixture.outcomes().await,
            vec![
                (AutomationOutcome::Admitted, key.clone()),
                (AutomationOutcome::Fired, key)
            ]
        );
        assert_eq!(log[1].job.as_deref(), Some(job.job_id.as_str()));
        // Charged once, not again for its end.
        let guards = fixture
            .automation
            .ledger()
            .guard_state("swap-on-limit", "%1", "", OffsetDateTime::now_utc())
            .await;
        assert_eq!(guards.global_fired_last_hour, 1);
        assert_eq!(guards.fired_last_hour, 1);
        assert!(fixture.engine.swap_inflight.is_empty());
        assert!(fixture.sends.lock().unwrap().is_empty());
    }

    /// A rule disabled while its admitted swap waited for the pane starts
    /// nothing, and its admission is released, not charged.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn an_automation_swap_disabled_while_it_waited_for_its_pane_is_dropped() {
        let mut fixture = SwapFixture::new().await;
        fixture.engine.rescan().await;
        let held = fixture.collaboration.worker_action_guard("%1").await;
        tokio::time::timeout(std::time::Duration::from_secs(2), fixture.engine.fire_due())
            .await
            .expect("a held pane must not hold the engine");
        // The engine still serves other work meanwhile.
        fixture.engine.rescan().await;
        fixture
            .automation
            .set_rule_enabled("swap-on-limit", false)
            .await
            .unwrap();
        drop(held);
        fixture.settle().await;
        assert_eq!(
            count(&fixture.snapshots),
            0,
            "a stale firing must not reach the swap service"
        );
        assert_eq!(count(&fixture.runs), 0);
        let key = Some(fixture.key.clone());
        assert_eq!(
            fixture.outcomes().await,
            vec![
                (AutomationOutcome::Admitted, key.clone()),
                (AutomationOutcome::Skipped, key)
            ]
        );
        let guards = fixture
            .automation
            .ledger()
            .guard_state("swap-on-limit", "%1", "", OffsetDateTime::now_utc())
            .await;
        assert_eq!(guards.global_fired_last_hour, 0);
        assert!(fixture.sends.lock().unwrap().is_empty());
    }

    /// A pane with a swap under way, or no room left, defers a firing
    /// instead of waiting on it or starting another.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn an_automation_swap_without_room_is_deferred() {
        let mut fixture = SwapFixture::new().await;
        let firing = |pane: &str| PlannedFiring {
            rule: "swap-on-limit".into(),
            pane: pane.into(),
            socket: Some("default".into()),
            agent_session_id: "capped".into(),
            agent: AgentKind::ClaudeCode,
            episode: "episode".into(),
            fire_at: OffsetDateTime::now_utc(),
            action: AutomationAction::SwapAccount,
            text: None,
            submit: false,
            only_if_still: tend::automation::AutomationCondition::Idle,
            account: None,
        };
        let rule = fixture.automation.config().await.rule[0].clone();
        let busy = |pane: &str| SwapOperation {
            admitted: ledger_entry(
                &firing(pane),
                OffsetDateTime::now_utc(),
                AutomationOutcome::Admitted,
                None,
            ),
            _drain_reservation: None,
        };
        fixture.engine.swap_inflight.insert("a".into(), busy("%1"));
        assert!(matches!(
            fixture
                .engine
                .admit_swap(&firing("%1"), &rule, 0, None, false, &mut None)
                .await,
            Admission::Deferred
        ));
        fixture.engine.swap_inflight.clear();
        fixture.engine.swap_inflight.insert("a".into(), busy("%7"));
        fixture.engine.swap_inflight.insert("b".into(), busy("%8"));
        assert!(matches!(
            fixture
                .engine
                .admit_swap(&firing("%1"), &rule, 0, None, false, &mut None)
                .await,
            Admission::Deferred
        ));
        assert!(fixture.automation.ledger().all().await.is_empty());
        assert_eq!(count(&fixture.snapshots), 0);
    }

    /// An engine that stopped while its swap ran leaves the operation on
    /// record; the next one follows that same job to its end, recording it
    /// once, and never starts it again.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_new_engine_follows_an_admitted_swap_it_did_not_start() {
        let mut fixture = SwapFixture::new().await;
        fixture.hold(true);
        fixture.engine.rescan().await;
        fixture.engine.fire_due().await;
        fixture.wait_for_runs(1).await;
        // The engine goes away; its task with it, not the job.
        drop(fixture.take_engine());
        assert_eq!(fixture.job().state, "running");
        let mut engine = AutomationEngine::new(
            fixture.automation.clone(),
            tend::Store::shared(),
            Vec::new(),
        );
        engine.swap_service = Some(Arc::clone(&fixture.swap));
        engine.reattach_swaps().await;
        assert_eq!(engine.swap_inflight.len(), 1);
        // Reattaching twice follows it once.
        engine.reattach_swaps().await;
        assert_eq!(engine.swap_ops.len(), 1);
        fixture.engine = engine;
        fixture.hold(false);
        fixture.settle().await;
        let key = Some(fixture.key.clone());
        assert_eq!(
            fixture.outcomes().await,
            vec![
                (AutomationOutcome::Admitted, key.clone()),
                (AutomationOutcome::Fired, key)
            ]
        );
        assert_eq!(count(&fixture.runs), 1);
        assert!(fixture
            .automation
            .ledger()
            .unresolved_operations()
            .await
            .is_empty());
    }

    /// A task that fails loses nothing: its operation is followed again by
    /// its key, never started again; a judged swap's end keeps its judgment.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_failed_swap_task_is_followed_again_and_keeps_its_judgment() {
        let mut fixture = SwapFixture::new().await;
        let judged = serde_json::json!({
            "judgment": {"decision": "match"},
            "condition": "is it stuck?",
            "observe_only": false,
            "execution": "admitted",
        })
        .to_string();
        let admitted = AutomationLedgerEntry {
            rule: "swap-on-limit".into(),
            pane: "%1".into(),
            agent: AgentKind::ClaudeCode,
            fired_at: OffsetDateTime::now_utc(),
            action: AutomationAction::SwapAccount,
            outcome: AutomationOutcome::Admitted,
            detail: Some(judged),
            episode: Some("episode".into()),
            operation: Some(fixture.key.clone()),
            job: None,
        };
        fixture
            .automation
            .ledger()
            .reserve_operation(admitted.clone(), |_| Ok(()))
            .await
            .unwrap();
        fixture.engine.swap_inflight.insert(
            fixture.key.clone(),
            SwapOperation {
                admitted,
                _drain_reservation: None,
            },
        );
        let task = fixture
            .engine
            .swap_ops
            .spawn(async { panic!("the operation's task failed") });
        fixture
            .engine
            .swap_tasks
            .insert(task.id(), fixture.key.clone());
        // The failure, then the operation followed again to its end.
        fixture.settle().await;
        assert_eq!(fixture.engine.swap_ops.len(), 1);
        fixture.settle().await;
        let log = fixture.automation.ledger().all().await;
        assert_eq!(log.len(), 2);
        assert_eq!(log[1].outcome, AutomationOutcome::Skipped);
        let detail: serde_json::Value =
            serde_json::from_str(log[1].detail.as_deref().unwrap()).unwrap();
        assert_eq!(detail["condition"], "is it stuck?");
        assert_eq!(detail["execution"], "no job was accepted under its key");
        assert_eq!(count(&fixture.runs), 0, "nothing was started again");
        assert!(fixture.engine.swap_inflight.is_empty());
    }

    /// An engine that stops while its swap's submission is still planning
    /// does not take the job's absence for "nothing accepted": the next one
    /// waits for that submission, then follows the job it made.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_reattached_operation_waits_for_a_submission_still_planning() {
        let mut fixture = SwapFixture::new().await;
        fixture.hold_planning(true);
        fixture.engine.rescan().await;
        fixture.engine.fire_due().await;
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while count(&fixture.snapshots) == 0 {
            assert!(
                std::time::Instant::now() < deadline,
                "planning never started"
            );
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        drop(fixture.take_engine());
        // No job yet: the submission is still planning.
        assert!(tend::swap_api::SwapJobs::status(
            fixture.service.as_ref(),
            &tend::swap_api::JobRef::Key(fixture.key.clone())
        )
        .is_err());
        let mut engine = AutomationEngine::new(
            fixture.automation.clone(),
            tend::Store::shared(),
            Vec::new(),
        );
        engine.swap_service = Some(Arc::clone(&fixture.swap));
        engine.reattach_swaps().await;
        fixture.engine = engine;
        // The follower waits instead of releasing it.
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        assert_eq!(
            fixture.outcomes().await.len(),
            1,
            "nothing may end it while its submission can still accept"
        );
        fixture.hold_planning(false);
        fixture.settle().await;
        let key = Some(fixture.key.clone());
        assert_eq!(
            fixture.outcomes().await,
            vec![
                (AutomationOutcome::Admitted, key.clone()),
                (AutomationOutcome::Fired, key)
            ]
        );
        assert_eq!(count(&fixture.runs), 1);
    }

    /// The claim on a key exists from admission, before the operation is
    /// ever polled: an operation dropped short of its submission settles
    /// the key only once it is gone, and then never submits.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn an_operation_dropped_before_its_submission_never_submits() {
        let mut fixture = SwapFixture::new().await;
        fixture.engine.rescan().await;
        // Armed while free; held now, so the operation waits for the pane.
        let held = fixture.collaboration.worker_action_guard("%1").await;
        let key = fixture.key.clone();
        fixture.engine.fire_due().await;
        assert!(
            fixture
                .swap
                .submissions
                .0
                .lock()
                .unwrap()
                .contains_key(&key),
            "claimed at admission"
        );
        drop(fixture.take_engine());
        tokio::time::timeout(
            std::time::Duration::from_secs(5),
            fixture.swap.submissions.settled(&key),
        )
        .await
        .expect("the dropped operation releases its claim");
        drop(held);
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        assert!(tend::swap_api::SwapJobs::status(
            fixture.service.as_ref(),
            &tend::swap_api::JobRef::Key(key)
        )
        .is_err());
        assert_eq!(count(&fixture.snapshots), 0);
        assert_eq!(count(&fixture.runs), 0);
    }

    /// While a swap holds its pane, another rule on that pane neither
    /// holds the engine nor waits: a notice goes out without the pane, a
    /// keystroke waits its turn in the queue.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_held_pane_defers_other_actions_without_holding_the_engine() {
        let mut fixture = SwapFixture::new().await;
        let held = fixture.collaboration.worker_action_guard("%1").await;
        let firing = |action: AutomationAction| PlannedFiring {
            rule: "other".into(),
            pane: "%1".into(),
            socket: Some("default".into()),
            agent_session_id: "capped".into(),
            agent: AgentKind::ClaudeCode,
            episode: "episode".into(),
            fire_at: OffsetDateTime::now_utc(),
            action,
            text: Some("continue".into()),
            submit: true,
            only_if_still: tend::automation::AutomationCondition::Idle,
            account: None,
        };
        let performed = tokio::time::timeout(std::time::Duration::from_secs(2), async {
            (
                fixture
                    .engine
                    .perform(&firing(AutomationAction::Notify))
                    .await,
                fixture
                    .engine
                    .perform(&firing(AutomationAction::SendPrompt))
                    .await,
            )
        })
        .await
        .expect("a held pane must not hold the engine");
        assert!(matches!(performed.0, Performed::Done));
        assert!(matches!(performed.1, Performed::Busy));
        // Arming on the held pane is skipped, not waited for.
        tokio::time::timeout(std::time::Duration::from_secs(2), fixture.engine.rescan())
            .await
            .expect("arming must not wait on a held pane");
        assert!(fixture.engine.pending.is_empty());
        drop(held);
        fixture.engine.rescan().await;
        assert_eq!(fixture.engine.pending.len(), 1);
        assert!(fixture.sends.lock().unwrap().is_empty());
    }

    /// An admission whose job was never accepted is released on reattach,
    /// with nothing sent.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_reattached_operation_with_no_job_is_released() {
        let mut fixture = SwapFixture::new().await;
        let mut admitted = AutomationLedgerEntry {
            rule: "swap-on-limit".into(),
            pane: "%1".into(),
            agent: AgentKind::ClaudeCode,
            fired_at: OffsetDateTime::now_utc(),
            action: AutomationAction::SwapAccount,
            outcome: AutomationOutcome::Admitted,
            detail: None,
            episode: Some("episode".into()),
            operation: None,
            job: None,
        };
        admitted.operation = Some(fixture.key.clone());
        fixture
            .automation
            .ledger()
            .reserve_operation(admitted, |_| Ok(()))
            .await
            .unwrap();
        fixture.engine.reattach_swaps().await;
        fixture.settle().await;
        assert_eq!(
            fixture.outcomes().await.last().unwrap().0,
            AutomationOutcome::Skipped
        );
        assert_eq!(count(&fixture.runs), 0);
        assert_eq!(count(&fixture.snapshots), 0);
    }
}
