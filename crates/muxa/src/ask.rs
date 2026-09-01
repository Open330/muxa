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
use tokio::sync::{watch, Mutex, RwLock};

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
    #[error("this conversation is still answering; wait for the current reply before sending another message")]
    ConversationBusy,
    #[error("ask conversation {0:?} was not found")]
    ConversationNotFound(String),
    #[error("ask agent {0:?} is not supported (use claude or codex)")]
    UnsupportedAgent(String),
    #[error("the supplied API key is for {supplied}, but the selected ask agent is {selected}")]
    CredentialAgentMismatch { supplied: String, selected: String },
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

/// One-turn provider credential. It is accepted only over the owner-only IPC
/// socket, moved directly into the selected child process environment, and is
/// never retained in [`AskEntry`] or [`AskSnapshot`].
#[derive(Clone, Deserialize)]
pub struct AskCredential {
    pub agent: String,
    pub api_key: String,
}

impl std::fmt::Debug for AskCredential {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("AskCredential")
            .field("agent", &self.agent)
            .field("api_key", &"<redacted>")
            .finish()
    }
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
    /// Muxa-owned conversation id. Unlike `agent_session_id`, this is stable
    /// before the first provider turn and is safe to expose as a UI identity.
    #[serde(default)]
    pub conversation_id: Option<String>,
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

/// A resumable Global Ask conversation. Provider session ids remain an
/// implementation detail while this muxa-owned id gives native clients a
/// durable conversation picker.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AskConversation {
    pub id: String,
    pub title: String,
    pub agent: String,
    #[serde(default)]
    pub agent_session_id: Option<String>,
    #[serde(with = "time::serde::rfc3339")]
    pub created_at: OffsetDateTime,
    #[serde(with = "time::serde::rfc3339")]
    pub updated_at: OffsetDateTime,
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
    conversations: Vec<AskConversation>,
    #[serde(default)]
    active_conversations: std::collections::HashMap<String, String>,
    #[serde(default)]
    entries: Vec<AskEntry>,
}

impl AskSnapshot {
    /// Upgrade the former provider -> session snapshot in place. Completed
    /// entries already retain the provider session id, so old resets naturally
    /// become separate conversations without discarding any history.
    fn migrate_conversations(&mut self) {
        for entry in &mut self.entries {
            let existing = entry.conversation_id.as_ref().and_then(|id| {
                self.conversations
                    .iter()
                    .position(|conversation| &conversation.id == id)
            });
            let matching_session = entry.agent_session_id.as_ref().and_then(|session| {
                self.conversations.iter().position(|conversation| {
                    conversation.agent == entry.agent
                        && conversation.agent_session_id.as_ref() == Some(session)
                })
            });
            let index = existing.or(matching_session).unwrap_or_else(|| {
                let conversation = AskConversation {
                    id: format!("conversation_{:x}", next_id()),
                    title: conversation_title(&entry.prompt),
                    agent: entry.agent.clone(),
                    agent_session_id: entry.agent_session_id.clone(),
                    created_at: entry.asked_at,
                    updated_at: entry.answered_at.unwrap_or(entry.asked_at),
                };
                self.conversations.push(conversation);
                self.conversations.len() - 1
            });
            let conversation = &mut self.conversations[index];
            if conversation.title.trim().is_empty() || conversation.title == "New conversation" {
                conversation.title = conversation_title(&entry.prompt);
            }
            if entry.agent_session_id.is_some() {
                conversation
                    .agent_session_id
                    .clone_from(&entry.agent_session_id);
            }
            conversation.created_at = conversation.created_at.min(entry.asked_at);
            conversation.updated_at = conversation
                .updated_at
                .max(entry.answered_at.unwrap_or(entry.asked_at));
            entry.conversation_id = Some(conversation.id.clone());
        }

        for (agent, session) in self.threads.clone() {
            let existing_id = self
                .conversations
                .iter()
                .filter(|conversation| {
                    conversation.agent == agent
                        && conversation.agent_session_id.as_deref() == Some(session.as_str())
                })
                .max_by_key(|conversation| conversation.updated_at)
                .map(|conversation| conversation.id.clone());
            let id = existing_id.unwrap_or_else(|| {
                let now = OffsetDateTime::now_utc();
                let conversation = AskConversation {
                    id: format!("conversation_{:x}", next_id()),
                    title: "Previous conversation".into(),
                    agent: agent.clone(),
                    agent_session_id: Some(session),
                    created_at: now,
                    updated_at: now,
                };
                let id = conversation.id.clone();
                self.conversations.push(conversation);
                id
            });
            self.active_conversations.entry(agent).or_insert(id);
        }

        for agent in ["claude", "codex"] {
            if self.active_conversations.contains_key(agent) {
                continue;
            }
            if let Some(id) = self
                .conversations
                .iter()
                .filter(|conversation| conversation.agent == agent)
                .max_by_key(|conversation| conversation.updated_at)
                .map(|conversation| conversation.id.clone())
            {
                self.active_conversations.insert(agent.into(), id);
            }
        }
    }
}

/// Durable ask history plus the id of the conversation still in progress.
pub struct AskStore {
    opts: AskOptions,
    entries: RwLock<Vec<AskEntry>>,
    conversations: RwLock<Vec<AskConversation>>,
    active_conversations: RwLock<std::collections::HashMap<String, String>>,
    /// Kept in the snapshot for rollback compatibility with older muxad
    /// builds. New code derives it from the currently selected conversation.
    threads: RwLock<std::collections::HashMap<String, String>>,
    /// Agent the next question goes to. Starts at the configured one and
    /// follows whatever the user picks in the panel.
    agent: RwLock<String>,
    /// Serializes each mutation with its snapshot write, so a reader
    /// never sees an entry the file does not have.
    write_lock: Mutex<()>,
    /// Monotonic content-free invalidation for native Ask clients.
    changes: watch::Sender<u64>,
}

impl AskStore {
    #[must_use]
    pub fn in_memory(opts: AskOptions) -> Arc<Self> {
        let agent = opts.agent.clone();
        let (changes, _) = watch::channel(0);
        Arc::new(Self {
            opts: AskOptions { path: None, ..opts },
            entries: RwLock::new(Vec::new()),
            conversations: RwLock::new(Vec::new()),
            active_conversations: RwLock::new(std::collections::HashMap::new()),
            threads: RwLock::new(std::collections::HashMap::new()),
            agent: RwLock::new(agent),
            write_lock: Mutex::new(()),
            changes,
        })
    }

    /// Read the snapshot back, converting any `running` leftovers into
    /// failures: their child process died with the previous daemon.
    // `async` for symmetry with `CollaborationStore::load` and so the
    // daemon's startup path reads the same for both stores.
    // Rust 1.98 renamed/expanded this lint. Keep the async API stable for
    // startup symmetry while supporting both the MSRV and current stable.
    #[allow(unknown_lints)]
    #[allow(clippy::unused_async, clippy::unused_async_trait_impl)]
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
        snapshot.migrate_conversations();
        let agent = opts.agent.clone();
        let (changes, _) = watch::channel(0);
        let store = Arc::new(Self {
            opts,
            entries: RwLock::new(snapshot.entries),
            conversations: RwLock::new(snapshot.conversations),
            active_conversations: RwLock::new(snapshot.active_conversations),
            threads: RwLock::new(snapshot.threads),
            agent: RwLock::new(agent),
            write_lock: Mutex::new(()),
            changes,
        });
        // Persist the normalized shape immediately. Otherwise a legacy file
        // with no subsequent Ask mutation would mint different muxa-owned
        // conversation ids on every daemon restart.
        store.persist().await;
        store
    }

    #[must_use]
    pub fn enabled(&self) -> bool {
        self.opts.enabled
    }

    pub fn subscribe(&self) -> watch::Receiver<u64> {
        self.changes.subscribe()
    }

    fn publish_change(&self) {
        self.changes
            .send_modify(|revision| *revision = revision.wrapping_add(1));
    }

    pub async fn list(&self) -> Vec<AskEntry> {
        self.entries.read().await.clone()
    }

    pub async fn list_conversations(&self) -> Vec<AskConversation> {
        let mut conversations = self.conversations.read().await.clone();
        conversations.sort_by(|left, right| right.updated_at.cmp(&left.updated_at));
        conversations
    }

    pub async fn active_conversation(&self) -> Option<AskConversation> {
        let agent = self.agent.read().await.clone();
        let id = self
            .active_conversations
            .read()
            .await
            .get(&agent)
            .cloned()?;
        self.conversations
            .read()
            .await
            .iter()
            .find(|conversation| conversation.id == id)
            .cloned()
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
        let mut selected = self.agent.write().await;
        let changed = *selected != label;
        selected.clone_from(&label);
        drop(selected);
        if changed {
            self.publish_change();
        }
        Ok(label)
    }

    /// Create and select a fresh conversation. History is kept and can be
    /// selected again later, including the provider session needed to resume.
    pub async fn reset_thread(&self) -> AskConversation {
        let _guard = self.write_lock.lock().await;
        let agent = self.agent.read().await.clone();
        self.threads.write().await.remove(&agent);
        let now = OffsetDateTime::now_utc();
        let conversation = AskConversation {
            id: format!("conversation_{:x}", next_id()),
            title: "New conversation".into(),
            agent: agent.clone(),
            agent_session_id: None,
            created_at: now,
            updated_at: now,
        };
        self.conversations.write().await.push(conversation.clone());
        self.active_conversations
            .write()
            .await
            .insert(agent, conversation.id.clone());
        self.persist().await;
        self.publish_change();
        conversation
    }

    pub async fn select_conversation(
        &self,
        conversation_id: &str,
    ) -> Result<AskConversation, AskError> {
        let _guard = self.write_lock.lock().await;
        let conversation = self
            .conversations
            .read()
            .await
            .iter()
            .find(|conversation| conversation.id == conversation_id)
            .cloned()
            .ok_or_else(|| AskError::ConversationNotFound(conversation_id.to_string()))?;
        self.agent.write().await.clone_from(&conversation.agent);
        self.active_conversations
            .write()
            .await
            .insert(conversation.agent.clone(), conversation.id.clone());
        let mut threads = self.threads.write().await;
        if let Some(session) = &conversation.agent_session_id {
            threads.insert(conversation.agent.clone(), session.clone());
        } else {
            threads.remove(&conversation.agent);
        }
        drop(threads);
        self.persist().await;
        self.publish_change();
        Ok(conversation)
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
        if removed > 0 {
            self.publish_change();
        }
        removed
    }

    /// Remove one completed history entry by id. Running entries are protected
    /// for the same reason as in [`Self::clear_history`]: their worker still
    /// needs a destination for its eventual result.
    pub async fn delete_history_entry(&self, id: &str) -> bool {
        let _guard = self.write_lock.lock().await;
        let mut entries = self.entries.write().await;
        let Some(index) = entries
            .iter()
            .position(|entry| entry.id == id && entry.status != AskStatus::Running)
        else {
            return false;
        };
        entries.remove(index);
        drop(entries);
        self.persist().await;
        self.publish_change();
        true
    }

    /// Record the question, spawn the agent, and return the pending entry
    /// immediately. The caller gets an id to watch; the answer arrives in
    /// the store when the child exits.
    pub async fn ask(self: &Arc<Self>, prompt: &str) -> Result<AskEntry, AskError> {
        self.ask_with_credential(prompt, None).await
    }

    /// Queue a question with an optional one-turn API key. The key lives only
    /// in the worker future and the child environment; persistence happens
    /// before it is moved into that future and contains no credential field.
    pub async fn ask_with_credential(
        self: &Arc<Self>,
        prompt: &str,
        credential: Option<AskCredential>,
    ) -> Result<AskEntry, AskError> {
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
        let api_key = match credential {
            Some(credential) if credential.agent == agent.label() => Some(credential.api_key),
            Some(credential) => {
                return Err(AskError::CredentialAgentMismatch {
                    supplied: credential.agent,
                    selected: agent.label().to_string(),
                });
            }
            None => None,
        };

        let write_guard = self.write_lock.lock().await;
        let conversation = self.ensure_active_conversation(agent.label()).await;
        let conversation_id = conversation.id.clone();
        if self.entries.read().await.iter().any(|entry| {
            entry.conversation_id.as_deref() == Some(conversation_id.as_str())
                && entry.status == AskStatus::Running
        }) {
            return Err(AskError::ConversationBusy);
        }
        let resume = conversation.agent_session_id.clone();
        let now = OffsetDateTime::now_utc();
        let entry = AskEntry {
            id: format!("ask_{:x}", next_id()),
            conversation_id: Some(conversation_id.clone()),
            prompt: prompt.to_string(),
            answer: String::new(),
            status: AskStatus::Running,
            agent: agent.label().to_string(),
            agent_session_id: resume.clone(),
            cwd: self.opts.cwd.display().to_string(),
            asked_at: now,
            answered_at: None,
            cost_usd: None,
            error: None,
        };

        {
            let mut entries = self.entries.write().await;
            entries.push(entry.clone());
            let keep = self.opts.keep.max(1);
            let excess = entries.len().saturating_sub(keep);
            entries.drain(..excess);
            drop(entries);
            if let Some(active) = self
                .conversations
                .write()
                .await
                .iter_mut()
                .find(|item| item.id == conversation_id)
            {
                if active.title == "New conversation" {
                    active.title = conversation_title(prompt);
                }
                active.updated_at = now;
            }
            self.persist().await;
        }
        self.publish_change();
        drop(write_guard);

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
                    api_key.as_deref(),
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
                .and_then(|e| {
                    e.agent_session_id.clone().map(|session| {
                        (
                            e.conversation_id.clone(),
                            e.agent.clone(),
                            session,
                            e.answered_at.unwrap_or(e.asked_at),
                        )
                    })
                })
        };
        if let Some((Some(conversation_id), agent, session, updated_at)) = advanced {
            if let Some(conversation) = self
                .conversations
                .write()
                .await
                .iter_mut()
                .find(|conversation| conversation.id == conversation_id)
            {
                conversation.agent_session_id = Some(session.clone());
                conversation.updated_at = updated_at;
            }
            if self
                .active_conversations
                .read()
                .await
                .get(&agent)
                .is_some_and(|active| active == &conversation_id)
            {
                self.threads.write().await.insert(agent, session);
            }
        }
        self.persist().await;
        self.publish_change();
    }

    async fn ensure_active_conversation(&self, agent: &str) -> AskConversation {
        if let Some(id) = self.active_conversations.read().await.get(agent).cloned() {
            if let Some(conversation) = self
                .conversations
                .read()
                .await
                .iter()
                .find(|conversation| conversation.id == id)
                .cloned()
            {
                return conversation;
            }
        }
        let now = OffsetDateTime::now_utc();
        let conversation = AskConversation {
            id: format!("conversation_{:x}", next_id()),
            title: "New conversation".into(),
            agent: agent.to_string(),
            agent_session_id: None,
            created_at: now,
            updated_at: now,
        };
        self.conversations.write().await.push(conversation.clone());
        self.active_conversations
            .write()
            .await
            .insert(agent.to_string(), conversation.id.clone());
        conversation
    }

    /// Snapshot to disk. Best-effort: an unwritable path degrades to an
    /// in-memory history rather than failing the query the user asked for.
    async fn persist(&self) {
        let Some(path) = self.opts.path.as_ref() else {
            return;
        };
        let snapshot = AskSnapshot {
            threads: self.threads.read().await.clone(),
            conversations: self.conversations.read().await.clone(),
            active_conversations: self.active_conversations.read().await.clone(),
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

fn conversation_title(prompt: &str) -> String {
    let flattened = prompt.split_whitespace().collect::<Vec<_>>().join(" ");
    let title = flattened.chars().take(56).collect::<String>();
    if title.is_empty() {
        "New conversation".into()
    } else if flattened.chars().count() > 56 {
        format!("{title}…")
    } else {
        title
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

/// One finished headless turn: the agent's final text, the conversation id
/// that would resume it, and what the turn cost.
#[derive(Debug, Clone)]
pub struct AskAnswer {
    pub text: String,
    pub session_id: Option<String>,
    pub cost_usd: Option<f64>,
}

/// One headless agent turn that answers to its caller instead of to a
/// store — the shape [`crate::pipeline`] uses to turn a work id into
/// ticket context.
///
/// [`AskStore::ask`] is the interactive path: it records the question,
/// returns immediately, and drops the answer into durable history for
/// whichever surface reads it next. A resolver wants the opposite —
/// nothing retained, the answer in hand before the next line of code
/// runs — so it borrows the same argv and parsing without the store, the
/// thread, or the `[ask].enabled` grant. Enabling `[ask]` is a grant to
/// answer *the user's* typed questions; a resolver runs because the user
/// typed `muxa work up`, which is its own consent.
#[derive(Debug, Clone)]
pub struct OneShot<'a> {
    /// `claude` or `codex`.
    pub agent: &'a str,
    pub prompt: &'a str,
    pub cwd: &'a std::path::Path,
    pub permission_mode: AskPermissionMode,
    pub additional_dirs: &'a [PathBuf],
    pub timeout: Duration,
}

/// Agent CLIs this bridge can drive headlessly, in preference order.
///
/// Membership is not "muxa knows this agent" — the launcher knows more
/// (gemini, agy, opencode) — but "it has a print mode that reports
/// completion as a fact": an exit code plus a parseable envelope. Without
/// that, reading an answer back means screen-scraping a moving target.
#[must_use]
pub fn supported_agents() -> &'static [&'static str] {
    &["claude", "codex"]
}

/// Run one headless turn and return its answer.
///
/// # Errors
/// Returns [`AskError::UnsupportedAgent`] for an agent CLI without a print
/// mode, [`AskError::EmptyPrompt`] for a blank prompt, and
/// [`AskError::Io`] when the child fails, times out, or answers with
/// something that is not a parseable result envelope.
pub async fn one_shot(request: OneShot<'_>) -> Result<AskAnswer, AskError> {
    let prompt = request.prompt.trim();
    if prompt.is_empty() {
        return Err(AskError::EmptyPrompt);
    }
    let agent = AskAgent::parse(request.agent)
        .ok_or_else(|| AskError::UnsupportedAgent(request.agent.to_string()))?;
    agent
        .run(
            prompt,
            None,
            request.cwd,
            request.permission_mode,
            request.additional_dirs,
            request.timeout.max(Duration::from_secs(5)),
            None,
        )
        .await
        .map_err(AskError::Io)
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

    #[allow(clippy::too_many_arguments)]
    async fn run(
        self,
        prompt: &str,
        resume: Option<&str>,
        cwd: &std::path::Path,
        permission_mode: AskPermissionMode,
        additional_dirs: &[PathBuf],
        timeout: Duration,
        api_key: Option<&str>,
    ) -> Result<AskAnswer, String> {
        let (bin, args) = self.argv(prompt, resume, permission_mode, additional_dirs);
        let mut cmd = tokio::process::Command::new(bin);
        cmd.args(&args)
            .current_dir(cwd)
            .stdin(std::process::Stdio::null())
            .kill_on_drop(true);
        if let Some(api_key) = api_key {
            cmd.env(self.api_key_environment(), api_key);
        }
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

    fn api_key_environment(self) -> &'static str {
        match self {
            Self::Claude => "ANTHROPIC_API_KEY",
            Self::Codex => "CODEX_API_KEY",
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
    use tempfile::tempdir;

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
    async fn prior_conversations_can_be_selected_and_resumed() {
        let store = AskStore::in_memory(AskOptions::default());
        let first = store.reset_thread().await;
        assert_eq!(first.agent, "claude");

        store.set_agent("codex").await.unwrap();
        let second = store.reset_thread().await;
        assert_eq!(second.agent, "codex");

        let selected = store.select_conversation(&first.id).await.unwrap();
        assert_eq!(selected.id, first.id);
        assert_eq!(store.agent().await, "claude");
        assert_eq!(store.active_conversation().await.unwrap().id, first.id);
        assert!(store
            .list_conversations()
            .await
            .iter()
            .any(|conversation| conversation.id == second.id));
    }

    #[test]
    fn legacy_provider_threads_migrate_into_durable_conversations() {
        let now = OffsetDateTime::now_utc();
        let mut snapshot = AskSnapshot {
            threads: std::collections::HashMap::from([("claude".into(), "session-1".into())]),
            entries: vec![AskEntry {
                id: "legacy".into(),
                conversation_id: None,
                prompt: "Review the release plan".into(),
                answer: "Ready".into(),
                status: AskStatus::Answered,
                agent: "claude".into(),
                agent_session_id: Some("session-1".into()),
                cwd: "/tmp".into(),
                asked_at: now,
                answered_at: Some(now),
                cost_usd: None,
                error: None,
            }],
            ..AskSnapshot::default()
        };

        snapshot.migrate_conversations();

        assert_eq!(snapshot.conversations.len(), 1);
        let conversation = &snapshot.conversations[0];
        assert_eq!(conversation.title, "Review the release plan");
        assert_eq!(conversation.agent_session_id.as_deref(), Some("session-1"));
        assert_eq!(
            snapshot.entries[0].conversation_id.as_deref(),
            Some(conversation.id.as_str())
        );
        assert_eq!(
            snapshot
                .active_conversations
                .get("claude")
                .map(String::as_str),
            Some(conversation.id.as_str())
        );
    }

    #[tokio::test]
    async fn loading_a_legacy_snapshot_persists_stable_conversation_ids() {
        let directory = tempdir().unwrap();
        let path = directory.path().join("ask.json");
        let legacy = serde_json::json!({
            "threads": { "claude": "session-1" },
            "entries": [{
                "id": "legacy",
                "prompt": "Keep this conversation",
                "answer": "Kept",
                "status": "answered",
                "agent": "claude",
                "agent_session_id": "session-1",
                "cwd": "/tmp",
                "asked_at": "2026-08-31T10:00:00Z",
                "answered_at": "2026-08-31T10:00:03Z"
            }]
        });
        std::fs::write(&path, serde_json::to_vec_pretty(&legacy).unwrap()).unwrap();

        let options = AskOptions {
            path: Some(path.clone()),
            ..AskOptions::default()
        };
        let first = AskStore::load(options.clone()).await;
        let first_id = first.active_conversation().await.unwrap().id;
        drop(first);

        let second = AskStore::load(options).await;
        assert_eq!(second.active_conversation().await.unwrap().id, first_id);
        let persisted: serde_json::Value =
            serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap();
        assert_eq!(persisted["conversations"].as_array().unwrap().len(), 1);
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
            conversation_id: None,
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
    async fn deleting_one_history_entry_refuses_running_work() {
        let store = AskStore::in_memory(AskOptions::default());
        let now = OffsetDateTime::now_utc();
        *store.entries.write().await = vec![
            AskEntry {
                id: "done".into(),
                conversation_id: None,
                prompt: "done".into(),
                answer: String::new(),
                status: AskStatus::Answered,
                agent: "claude".into(),
                agent_session_id: None,
                cwd: "/tmp".into(),
                asked_at: now,
                answered_at: Some(now),
                cost_usd: None,
                error: None,
            },
            AskEntry {
                id: "running".into(),
                conversation_id: None,
                prompt: "running".into(),
                answer: String::new(),
                status: AskStatus::Running,
                agent: "claude".into(),
                agent_session_id: None,
                cwd: "/tmp".into(),
                asked_at: now,
                answered_at: None,
                cost_usd: None,
                error: None,
            },
        ];

        assert!(!store.delete_history_entry("running").await);
        assert!(store.delete_history_entry("done").await);
        assert_eq!(
            store
                .list()
                .await
                .iter()
                .map(|entry| entry.id.as_str())
                .collect::<Vec<_>>(),
            vec!["running"]
        );
    }

    #[tokio::test]
    async fn an_unknown_agent_is_refused() {
        let store = AskStore::in_memory(AskOptions::default());
        assert!(store.set_agent("gemini").await.is_err());
        assert_eq!(store.agent().await, "claude");
    }

    #[test]
    fn one_turn_credentials_redact_the_secret_from_debug_output() {
        let credential = AskCredential {
            agent: "codex".into(),
            api_key: "must-never-appear".into(),
        };
        let debug = format!("{credential:?}");
        assert!(!debug.contains("must-never-appear"));
        assert!(debug.contains("<redacted>"));
    }

    #[tokio::test]
    async fn a_key_for_the_wrong_provider_is_refused_before_spawn() {
        let store = AskStore::in_memory(AskOptions {
            enabled: true,
            ..AskOptions::default()
        });
        let result = store
            .ask_with_credential(
                "hello",
                Some(AskCredential {
                    agent: "codex".into(),
                    api_key: "secret".into(),
                }),
            )
            .await;
        assert!(matches!(
            result,
            Err(AskError::CredentialAgentMismatch { .. })
        ));
        assert!(store.list().await.is_empty());
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
