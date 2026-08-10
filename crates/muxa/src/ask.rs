//! `muxa ask` — headless one-shot queries to an agent CLI, with the
//! answer captured instead of typed into a pane.
//!
//! **Why headless rather than a parked interactive session.** Keeping a
//! `claude`/`codex` TUI alive and typing into it is the obvious shape, and
//! it does not work: a TUI gives no machine-readable "the answer ends
//! here", so reading a reply back means screen-scraping a moving target.
//! Print mode (`claude -p --output-format json`, `codex exec --json`)
//! answers with structured output and an exit code — completion is a fact,
//! not a guess.
//!
//! **And it is not slower.** Both CLIs resume a prior conversation by id,
//! so the second question onward reuses the cached system context the
//! first one paid for, which is the efficiency a parked session was meant
//! to buy. The thread continues until the user resets it, and the entries
//! outlive the daemon because they live in the same durable-JSON shape the
//! collaboration mailbox uses.
//!
//! The daemon owns execution so a query survives the watch popup closing:
//! the answer lands in the store either way, and the next `muxa watch`
//! shows it.

use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use time::OffsetDateTime;
use tokio::sync::{Mutex, RwLock};

use crate::config::{AskPermissionMode, DEFAULT_ASK_TIMEOUT_SECS};

/// How many entries the store keeps. Old answers are worth re-reading;
/// unbounded growth is not.
const DEFAULT_KEEP: usize = 200;

#[derive(Debug, thiserror::Error)]
pub enum AskError {
    #[error("ask is disabled; enable [ask].enabled")]
    Disabled,
    #[error("ask prompt is empty")]
    EmptyPrompt,
    #[error("ask agent {0:?} is not supported (use claude or codex)")]
    UnsupportedAgent(String),
    #[error("{0}")]
    Io(String),
}

/// Runtime knobs, resolved from `[ask]` by the daemon.
#[derive(Debug, Clone)]
pub struct AskOptions {
    pub enabled: bool,
    pub agent: String,
    /// Working directory the headless process runs in. Default-mode asks are
    /// intended as queries; edit/bypass automation still resolves its files
    /// relative to this operator-selected root.
    pub cwd: PathBuf,
    pub permission_mode: AskPermissionMode,
    pub additional_dirs: Vec<PathBuf>,
    pub timeout_secs: u64,
    pub path: Option<PathBuf>,
    pub keep: usize,
}

impl Default for AskOptions {
    fn default() -> Self {
        Self {
            enabled: false,
            agent: "claude".into(),
            cwd: dirs::home_dir().unwrap_or_else(|| PathBuf::from(".")),
            permission_mode: AskPermissionMode::Bypass,
            additional_dirs: Vec::new(),
            timeout_secs: DEFAULT_ASK_TIMEOUT_SECS,
            path: None,
            keep: DEFAULT_KEEP,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AskStatus {
    /// Spawned, no answer yet. A `running` entry left behind by a daemon
    /// that died is re-labelled `failed` at load — a query cannot outlive
    /// the process that owns its child.
    Running,
    Answered,
    Failed,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AskEntry {
    pub id: String,
    pub prompt: String,
    #[serde(default)]
    pub answer: String,
    pub status: AskStatus,
    pub agent: String,
    /// The agent CLI's own conversation id, kept so the next question can
    /// resume this thread.
    #[serde(default)]
    pub agent_session_id: Option<String>,
    pub cwd: String,
    #[serde(with = "time::serde::rfc3339")]
    pub asked_at: OffsetDateTime,
    #[serde(default, with = "time::serde::rfc3339::option")]
    pub answered_at: Option<OffsetDateTime>,
    #[serde(default)]
    pub cost_usd: Option<f64>,
    #[serde(default)]
    pub error: Option<String>,
}

impl AskEntry {
    /// Wall-clock the query took, once it has finished.
    #[must_use]
    pub fn duration(&self) -> Option<Duration> {
        let end = self.answered_at?;
        (end - self.asked_at).try_into().ok()
    }
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct AskSnapshot {
    /// Conversation the next question resumes, per agent. Keyed rather
    /// than single because a claude session id means nothing to codex —
    /// switching agents has to switch threads, not corrupt one.
    #[serde(default)]
    threads: std::collections::HashMap<String, String>,
    #[serde(default)]
    entries: Vec<AskEntry>,
}

/// Durable ask history plus the id of the conversation still in progress.
pub struct AskStore {
    opts: AskOptions,
    entries: RwLock<Vec<AskEntry>>,
    threads: RwLock<std::collections::HashMap<String, String>>,
    /// Agent the next question goes to. Starts at the configured one and
    /// follows whatever the user picks in the panel.
    agent: RwLock<String>,
    /// Serializes each mutation with its snapshot write, so a reader
    /// never sees an entry the file does not have.
    write_lock: Mutex<()>,
}

impl AskStore {
    #[must_use]
    pub fn in_memory(opts: AskOptions) -> Arc<Self> {
        let agent = opts.agent.clone();
        Arc::new(Self {
            opts: AskOptions { path: None, ..opts },
            entries: RwLock::new(Vec::new()),
            threads: RwLock::new(std::collections::HashMap::new()),
            agent: RwLock::new(agent),
            write_lock: Mutex::new(()),
        })
    }

    /// Read the snapshot back, converting any `running` leftovers into
    /// failures: their child process died with the previous daemon.
    // `async` for symmetry with `CollaborationStore::load` and so the
    // daemon's startup path reads the same for both stores.
    #[allow(clippy::unused_async)]
    pub async fn load(opts: AskOptions) -> Arc<Self> {
        let mut snapshot = opts
            .path
            .as_ref()
            .and_then(|path| std::fs::read_to_string(path).ok())
            .and_then(|text| serde_json::from_str::<AskSnapshot>(&text).ok())
            .unwrap_or_default();
        for entry in &mut snapshot.entries {
            if entry.status == AskStatus::Running {
                entry.status = AskStatus::Failed;
                entry.error = Some("muxad restarted before the answer arrived".into());
                entry.answered_at = Some(OffsetDateTime::now_utc());
            }
        }
        let agent = opts.agent.clone();
        Arc::new(Self {
            opts,
            entries: RwLock::new(snapshot.entries),
            threads: RwLock::new(snapshot.threads),
            agent: RwLock::new(agent),
            write_lock: Mutex::new(()),
        })
    }

    #[must_use]
    pub fn enabled(&self) -> bool {
        self.opts.enabled
    }

    pub async fn list(&self) -> Vec<AskEntry> {
        self.entries.read().await.clone()
    }

    /// Agent the next question goes to.
    pub async fn agent(&self) -> String {
        self.agent.read().await.clone()
    }

    /// Point the next question at a different agent. Each agent keeps its
    /// own thread, so switching back resumes where that one left off
    /// rather than starting over.
    pub async fn set_agent(&self, agent: &str) -> Result<String, AskError> {
        let parsed =
            AskAgent::parse(agent).ok_or_else(|| AskError::UnsupportedAgent(agent.to_string()))?;
        let label = parsed.label().to_string();
        *self.agent.write().await = label.clone();
        Ok(label)
    }

    /// Drop the current agent's conversation id so its next question
    /// starts fresh. History is kept — resetting a thread is not
    /// forgetting — and the other agent's thread is left alone.
    pub async fn reset_thread(&self) {
        let _guard = self.write_lock.lock().await;
        let agent = self.agent.read().await.clone();
        self.threads.write().await.remove(&agent);
        self.persist().await;
    }

    /// Remove completed history while preserving active asks and conversation
    /// ids. A running child still needs its entry so [`Self::finish`] can
    /// publish the answer when it exits; starting a fresh conversation remains
    /// the separate responsibility of [`Self::reset_thread`].
    pub async fn clear_history(&self) -> usize {
        let _guard = self.write_lock.lock().await;
        let mut entries = self.entries.write().await;
        let before = entries.len();
        entries.retain(|entry| entry.status == AskStatus::Running);
        let removed = before.saturating_sub(entries.len());
        drop(entries);
        self.persist().await;
        removed
    }

    /// Record the question, spawn the agent, and return the pending entry
    /// immediately. The caller gets an id to watch; the answer arrives in
    /// the store when the child exits.
    pub async fn ask(self: &Arc<Self>, prompt: &str) -> Result<AskEntry, AskError> {
        if !self.opts.enabled {
            return Err(AskError::Disabled);
        }
        let prompt = prompt.trim();
        if prompt.is_empty() {
            return Err(AskError::EmptyPrompt);
        }
        let selected = self.agent.read().await.clone();
        let Some(agent) = AskAgent::parse(&selected) else {
            return Err(AskError::UnsupportedAgent(selected));
        };

        let resume = self.threads.read().await.get(agent.label()).cloned();
        let entry = AskEntry {
            id: format!("ask_{:x}", next_id()),
            prompt: prompt.to_string(),
            answer: String::new(),
            status: AskStatus::Running,
            agent: agent.label().to_string(),
            agent_session_id: resume.clone(),
            cwd: self.opts.cwd.display().to_string(),
            asked_at: OffsetDateTime::now_utc(),
            answered_at: None,
            cost_usd: None,
            error: None,
        };

        {
            let _guard = self.write_lock.lock().await;
            let mut entries = self.entries.write().await;
            entries.push(entry.clone());
            let keep = self.opts.keep.max(1);
            let excess = entries.len().saturating_sub(keep);
            entries.drain(..excess);
            drop(entries);
            self.persist().await;
        }

        let store = Arc::clone(self);
        let id = entry.id.clone();
        let prompt = prompt.to_string();
        tokio::spawn(async move {
            let outcome = agent
                .run(
                    &prompt,
                    resume.as_deref(),
                    &store.opts.cwd,
                    store.opts.permission_mode,
                    &store.opts.additional_dirs,
                    Duration::from_secs(store.opts.timeout_secs.max(5)),
                )
                .await;
            store.finish(&id, outcome).await;
        });

        Ok(entry)
    }

    async fn finish(&self, id: &str, outcome: Result<AskAnswer, String>) {
        let _guard = self.write_lock.lock().await;
        {
            let mut entries = self.entries.write().await;
            let Some(entry) = entries.iter_mut().find(|e| e.id == id) else {
                return;
            };
            entry.answered_at = Some(OffsetDateTime::now_utc());
            match outcome {
                Ok(answer) => {
                    entry.status = AskStatus::Answered;
                    entry.answer = answer.text;
                    entry.cost_usd = answer.cost_usd;
                    if answer.session_id.is_some() {
                        entry.agent_session_id.clone_from(&answer.session_id);
                    }
                }
                Err(error) => {
                    entry.status = AskStatus::Failed;
                    entry.error = Some(error);
                }
            }
        }
        // Continue the thread from whatever the agent just answered with.
        // Only a successful turn advances it: resuming a session that
        // errored out mid-turn is how a broken thread becomes permanent.
        let advanced = {
            let entries = self.entries.read().await;
            entries
                .iter()
                .find(|e| e.id == id)
                .filter(|e| e.status == AskStatus::Answered)
                .and_then(|e| e.agent_session_id.clone().map(|s| (e.agent.clone(), s)))
        };
        if let Some((agent, session)) = advanced {
            self.threads.write().await.insert(agent, session);
        }
        self.persist().await;
    }

    /// Snapshot to disk. Best-effort: an unwritable path degrades to an
    /// in-memory history rather than failing the query the user asked for.
    async fn persist(&self) {
        let Some(path) = self.opts.path.as_ref() else {
            return;
        };
        let snapshot = AskSnapshot {
            threads: self.threads.read().await.clone(),
            entries: self.entries.read().await.clone(),
        };
        let Ok(text) = serde_json::to_string_pretty(&snapshot) else {
            return;
        };
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        // Write-then-rename so a reader never catches a half-written file.
        let tmp = path.with_extension("json.tmp");
        if std::fs::write(&tmp, text).is_ok() {
            let _ = std::fs::rename(&tmp, path);
        }
    }
}

fn next_id() -> u64 {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let seq = COUNTER.fetch_add(1, Ordering::Relaxed);
    // Nanos truncated to 64 bits is fine: this is an opaque handle, not a
    // clock, and the counter breaks ties inside the same nanosecond.
    let now = u64::try_from(OffsetDateTime::now_utc().unix_timestamp_nanos()).unwrap_or(0);
    now.rotate_left(8) ^ seq
}

struct AskAnswer {
    text: String,
    session_id: Option<String>,
    cost_usd: Option<f64>,
}

#[derive(Debug, Clone, Copy)]
enum AskAgent {
    Claude,
    Codex,
}

impl AskAgent {
    fn parse(name: &str) -> Option<Self> {
        match name.trim().to_ascii_lowercase().as_str() {
            "claude" => Some(Self::Claude),
            "codex" => Some(Self::Codex),
            _ => None,
        }
    }

    fn label(self) -> &'static str {
        match self {
            Self::Claude => "claude",
            Self::Codex => "codex",
        }
    }

    /// Argv for one headless turn. `resume` continues an existing
    /// conversation; `None` starts a new one.
    fn argv(
        self,
        prompt: &str,
        resume: Option<&str>,
        permission_mode: AskPermissionMode,
        additional_dirs: &[PathBuf],
    ) -> (&'static str, Vec<String>) {
        match self {
            Self::Claude => {
                let mut args = vec!["-p".to_string(), "--output-format".into(), "json".into()];
                match permission_mode {
                    AskPermissionMode::Default => {}
                    AskPermissionMode::Edit => args.push("--permission-mode=acceptEdits".into()),
                    AskPermissionMode::Bypass => {
                        args.push("--dangerously-skip-permissions".into());
                    }
                }
                args.extend(
                    additional_dirs
                        .iter()
                        .map(|dir| format!("--add-dir={}", dir.display())),
                );
                if let Some(id) = resume {
                    args.push("--resume".into());
                    args.push(id.to_string());
                }
                args.push(prompt.to_string());
                ("claude", args)
            }
            Self::Codex => {
                // `exec` is codex's print mode; `exec resume <id>` continues
                // one. Both take the prompt as a trailing argument.
                let mut args = vec!["exec".to_string()];
                if let Some(id) = resume {
                    args.push("resume".into());
                    args.push(id.to_string());
                }
                match permission_mode {
                    AskPermissionMode::Default => {}
                    AskPermissionMode::Edit => {
                        args.push("--sandbox=workspace-write".into());
                        args.push("--approve-for-me".into());
                    }
                    AskPermissionMode::Bypass => {
                        args.push("--dangerously-bypass-approvals-and-sandbox".into());
                    }
                }
                args.extend(
                    additional_dirs
                        .iter()
                        .map(|dir| format!("--add-dir={}", dir.display())),
                );
                args.push("--json".into());
                args.push(prompt.to_string());
                ("codex", args)
            }
        }
    }

    async fn run(
        self,
        prompt: &str,
        resume: Option<&str>,
        cwd: &std::path::Path,
        permission_mode: AskPermissionMode,
        additional_dirs: &[PathBuf],
        timeout: Duration,
    ) -> Result<AskAnswer, String> {
        let (bin, args) = self.argv(prompt, resume, permission_mode, additional_dirs);
        let mut cmd = tokio::process::Command::new(bin);
        cmd.args(&args)
            .current_dir(cwd)
            .stdin(std::process::Stdio::null())
            .kill_on_drop(true);
        let output = tokio::time::timeout(timeout, cmd.output())
            .await
            .map_err(|_| {
                format!(
                    "{bin} exceeded the ask timeout after {}s; it may still have been working — increase [ask].timeout_secs for long-running tasks",
                    timeout.as_secs()
                )
            })?
            .map_err(|e| format!("spawning {bin}: {e}"))?;
        let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            let detail = stderr.trim().lines().next_back().unwrap_or("no stderr");
            return Err(format!("{bin} exited non-zero: {detail}"));
        }
        match self {
            Self::Claude => parse_claude_json(&stdout),
            Self::Codex => parse_codex_jsonl(&stdout),
        }
    }
}

/// `claude -p --output-format json` answers with one object carrying the
/// text, the conversation id to resume, and the turn's cost.
fn parse_claude_json(stdout: &str) -> Result<AskAnswer, String> {
    let value: serde_json::Value =
        serde_json::from_str(stdout.trim()).map_err(|e| format!("parsing claude JSON: {e}"))?;
    if value.get("is_error").and_then(serde_json::Value::as_bool) == Some(true) {
        let detail = value
            .get("result")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("claude reported an error");
        return Err(detail.to_string());
    }
    let text = value
        .get("result")
        .and_then(serde_json::Value::as_str)
        .ok_or("claude JSON has no result field")?
        .to_string();
    Ok(AskAnswer {
        text,
        session_id: value
            .get("session_id")
            .and_then(serde_json::Value::as_str)
            .map(str::to_string),
        cost_usd: value
            .get("total_cost_usd")
            .and_then(serde_json::Value::as_f64),
    })
}

/// `codex exec --json` streams one JSON event per line. The answer is the
/// last agent message; the session id appears in a configured/session
/// event near the top.
fn parse_codex_jsonl(stdout: &str) -> Result<AskAnswer, String> {
    let mut text: Option<String> = None;
    let mut session_id: Option<String> = None;
    for line in stdout.lines() {
        let Ok(value) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        for key in ["session_id", "conversation_id", "thread_id"] {
            if session_id.is_none() {
                session_id = find_str(&value, key);
            }
        }
        // Codex has moved this field around across versions; take the last
        // message-shaped payload rather than pinning one event name.
        for key in ["last_agent_message", "agent_message", "message", "text"] {
            if let Some(found) = find_str(&value, key) {
                if !found.trim().is_empty() {
                    text = Some(found);
                }
            }
        }
    }
    let text = text.ok_or("codex produced no agent message")?;
    Ok(AskAnswer {
        text,
        session_id,
        cost_usd: None,
    })
}

/// First string value for `key` anywhere in `value`. Codex nests its
/// payloads differently per event, and a recursive lookup is cheaper than
/// tracking every shape.
fn find_str(value: &serde_json::Value, key: &str) -> Option<String> {
    match value {
        serde_json::Value::Object(map) => {
            if let Some(serde_json::Value::String(s)) = map.get(key) {
                return Some(s.clone());
            }
            map.values().find_map(|v| find_str(v, key))
        }
        serde_json::Value::Array(items) => items.iter().find_map(|v| find_str(v, key)),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn claude_json_yields_text_session_and_cost() {
        let raw =
            r#"{"is_error":false,"result":"PONG","session_id":"abc-123","total_cost_usd":0.09}"#;
        let answer = parse_claude_json(raw).unwrap();
        assert_eq!(answer.text, "PONG");
        assert_eq!(answer.session_id.as_deref(), Some("abc-123"));
        assert!((answer.cost_usd.unwrap() - 0.09).abs() < f64::EPSILON);
    }

    #[test]
    fn claude_error_turns_into_an_error_not_an_answer() {
        let raw = r#"{"is_error":true,"result":"rate limited","session_id":"abc"}"#;
        assert!(parse_claude_json(raw).is_err());
    }

    #[test]
    fn codex_jsonl_takes_the_last_message_and_finds_the_session() {
        let raw = concat!(
            r#"{"type":"session.created","payload":{"session_id":"s-1"}}"#,
            "\n",
            r#"{"type":"item","payload":{"message":"first"}}"#,
            "\n",
            r#"{"type":"item","payload":{"message":"final answer"}}"#,
            "\n",
        );
        let answer = parse_codex_jsonl(raw).unwrap();
        assert_eq!(answer.text, "final answer");
        assert_eq!(answer.session_id.as_deref(), Some("s-1"));
    }

    #[test]
    fn claude_argv_only_resumes_when_there_is_a_thread() {
        let (bin, fresh) = AskAgent::Claude.argv("hi", None, AskPermissionMode::Default, &[]);
        assert_eq!(bin, "claude");
        assert!(!fresh.contains(&"--resume".to_string()));
        assert_eq!(fresh.last().unwrap(), "hi");

        let (_, resumed) =
            AskAgent::Claude.argv("hi", Some("s-9"), AskPermissionMode::Default, &[]);
        let at = resumed.iter().position(|a| a == "--resume").unwrap();
        assert_eq!(resumed[at + 1], "s-9");
    }

    #[test]
    fn execution_controls_are_explicit_in_agent_argv() {
        let dirs = [PathBuf::from("/nfs/home/june")];
        let (_, claude) = AskAgent::Claude.argv("resolve", None, AskPermissionMode::Bypass, &dirs);
        assert!(claude.contains(&"--dangerously-skip-permissions".to_string()));
        assert!(claude.contains(&"--add-dir=/nfs/home/june".to_string()));

        let (_, codex) = AskAgent::Codex.argv("resolve", None, AskPermissionMode::Bypass, &dirs);
        assert!(codex.contains(&"--dangerously-bypass-approvals-and-sandbox".to_string()));
        assert!(codex.contains(&"--add-dir=/nfs/home/june".to_string()));

        let (_, safe) = AskAgent::Claude.argv("question", None, AskPermissionMode::Default, &dirs);
        assert!(!safe.contains(&"--dangerously-skip-permissions".to_string()));
    }

    #[test]
    fn unattended_ask_defaults_support_resolver_workflows() {
        assert_eq!(
            AskOptions::default().permission_mode,
            AskPermissionMode::Bypass
        );
        assert_eq!(AskOptions::default().timeout_secs, DEFAULT_ASK_TIMEOUT_SECS);
    }

    #[tokio::test]
    async fn switching_agents_keeps_each_thread_separate() {
        let store = AskStore::in_memory(AskOptions {
            enabled: true,
            ..AskOptions::default()
        });
        assert_eq!(store.agent().await, "claude");
        store
            .threads
            .write()
            .await
            .insert("claude".into(), "c-1".into());

        assert_eq!(store.set_agent("codex").await.unwrap(), "codex");
        // codex must not inherit claude's session id — it means nothing
        // to the other CLI.
        assert!(store.threads.read().await.get("codex").is_none());
        store
            .threads
            .write()
            .await
            .insert("codex".into(), "x-1".into());

        // Resetting the current agent leaves the other thread intact.
        store.reset_thread().await;
        let threads = store.threads.read().await;
        assert!(threads.get("codex").is_none());
        assert_eq!(threads.get("claude").map(String::as_str), Some("c-1"));
    }

    #[tokio::test]
    async fn clearing_history_keeps_running_asks_and_conversation_ids() {
        let store = AskStore::in_memory(AskOptions::default());
        store
            .threads
            .write()
            .await
            .insert("claude".into(), "c-1".into());
        let now = OffsetDateTime::now_utc();
        let entry = |id: &str, status: AskStatus| AskEntry {
            id: id.into(),
            prompt: id.into(),
            answer: String::new(),
            status,
            agent: "claude".into(),
            agent_session_id: None,
            cwd: "/tmp".into(),
            asked_at: now,
            answered_at: (status != AskStatus::Running).then_some(now),
            cost_usd: None,
            error: None,
        };
        *store.entries.write().await = vec![
            entry("answered", AskStatus::Answered),
            entry("failed", AskStatus::Failed),
            entry("running", AskStatus::Running),
        ];

        assert_eq!(store.clear_history().await, 2);
        let entries = store.list().await;
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].id, "running");
        assert_eq!(
            store.threads.read().await.get("claude").map(String::as_str),
            Some("c-1")
        );
    }

    #[tokio::test]
    async fn an_unknown_agent_is_refused() {
        let store = AskStore::in_memory(AskOptions::default());
        assert!(store.set_agent("gemini").await.is_err());
        assert_eq!(store.agent().await, "claude");
    }

    #[tokio::test]
    async fn a_disabled_store_refuses_before_spawning_anything() {
        let store = AskStore::in_memory(AskOptions::default());
        assert!(matches!(store.ask("hi").await, Err(AskError::Disabled)));
    }

    #[tokio::test]
    async fn an_empty_prompt_is_refused() {
        let store = AskStore::in_memory(AskOptions {
            enabled: true,
            ..AskOptions::default()
        });
        assert!(matches!(store.ask("   ").await, Err(AskError::EmptyPrompt)));
    }
}
