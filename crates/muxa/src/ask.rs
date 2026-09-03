//! `muxa ask` — headless one-shot queries to an agent CLI or an LLM API,
//! with the answer captured instead of typed into a pane.
//!
//! **Why headless rather than a parked interactive session.** Keeping a
//! `claude`/`codex` TUI alive and typing into it is the obvious shape, and
//! it does not work: a TUI gives no machine-readable "the answer ends
//! here", so reading a reply back means screen-scraping a moving target.
//! Print mode (`claude -p --output-format json`, `codex exec --json`,
//! `gemini -p --output-format json`) answers with structured output and an
//! exit code — completion is a fact, not a guess.
//!
//! **And it is not slower.** The CLIs resume a prior conversation by id,
//! so the second question onward reuses the cached system context the
//! first one paid for, which is the efficiency a parked session was meant
//! to buy. The thread continues until the user resets it, and the entries
//! outlive the daemon because they live in the same durable-JSON shape the
//! collaboration mailbox uses.
//!
//! **API providers** (`anthropic`, `openai`) have no server-side thread to
//! resume, so muxa replays the conversation's prior turns from its own
//! store, most recent first up to a fixed budget, ahead of the new prompt.
//! Their key comes from the request, the daemon's environment, or an
//! environment variable named in `[ask.providers.<id>]` — never from the
//! config file itself.
//!
//! The daemon owns execution so a query survives the watch popup closing:
//! the answer lands in the store either way, and the next `muxa watch`
//! shows it.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;
use time::OffsetDateTime;
use tokio::sync::{watch, Mutex, RwLock};

use crate::config::{AskPermissionMode, AskProviderConfig, Config, DEFAULT_ASK_TIMEOUT_SECS};

/// How many entries the store keeps. Old answers are worth re-reading;
/// unbounded growth is not.
const DEFAULT_KEEP: usize = 200;

/// Ceiling on one API answer. Generous for a question, small next to the
/// context window, and the number every request body has to carry.
const API_MAX_TOKENS: u32 = 8192;

/// Replay budget for API providers: at most this many prior turns…
pub const REPLAY_MAX_TURNS: usize = 40;
/// …and at most this many characters of them, newest first.
pub const REPLAY_MAX_CHARS: usize = 60_000;

const ANTHROPIC_MESSAGES_URL: &str = "https://api.anthropic.com/v1/messages";
const ANTHROPIC_VERSION: &str = "2023-06-01";
const OPENAI_CHAT_URL: &str = "https://api.openai.com/v1/chat/completions";

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
    #[error("ask agent {0:?} is not supported (use claude, codex, gemini, anthropic, or openai)")]
    UnsupportedAgent(String),
    #[error("the supplied API key is for {supplied}, but the selected ask agent is {selected}")]
    CredentialAgentMismatch { supplied: String, selected: String },
    #[error("no config file is known for [ask.providers.{0}]; start muxad with --config or a default config path")]
    NoConfigPath(String),
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
    /// `[ask.providers.<id>]` as loaded; [`AskStore::configure_provider`]
    /// keeps the live copy in step with the file afterwards.
    pub providers: BTreeMap<String, AskProviderConfig>,
    /// The `config.toml` the daemon read `[ask]` from, so provider settings
    /// can be written back where they came from.
    pub config_path: Option<PathBuf>,
}

/// One-turn provider credential. It is accepted only over the owner-only IPC
/// socket, moved directly into the selected child process environment (or
/// the API request header), and is never retained in [`AskEntry`] or
/// [`AskSnapshot`].
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
            providers: BTreeMap::new(),
            config_path: None,
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
    /// resume this thread. Always absent for API providers.
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

        for agent in supported_agents() {
            if self.active_conversations.contains_key(*agent) {
                continue;
            }
            if let Some(id) = self
                .conversations
                .iter()
                .filter(|conversation| conversation.agent == *agent)
                .max_by_key(|conversation| conversation.updated_at)
                .map(|conversation| conversation.id.clone())
            {
                self.active_conversations.insert((*agent).into(), id);
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
    /// Live `[ask.providers.<id>]`, refreshed by [`Self::configure_provider`].
    providers: RwLock<BTreeMap<String, AskProviderConfig>>,
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
        let providers = opts.providers.clone();
        let (changes, _) = watch::channel(0);
        Arc::new(Self {
            opts: AskOptions { path: None, ..opts },
            entries: RwLock::new(Vec::new()),
            conversations: RwLock::new(Vec::new()),
            active_conversations: RwLock::new(std::collections::HashMap::new()),
            threads: RwLock::new(std::collections::HashMap::new()),
            agent: RwLock::new(agent),
            providers: RwLock::new(providers),
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
        let providers = opts.providers.clone();
        let (changes, _) = watch::channel(0);
        let store = Arc::new(Self {
            opts,
            entries: RwLock::new(snapshot.entries),
            conversations: RwLock::new(snapshot.conversations),
            active_conversations: RwLock::new(snapshot.active_conversations),
            threads: RwLock::new(snapshot.threads),
            agent: RwLock::new(agent),
            providers: RwLock::new(providers),
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
        conversations.sort_by_key(|conversation| std::cmp::Reverse(conversation.updated_at));
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
        let parsed = AskProvider::parse(agent)
            .ok_or_else(|| AskError::UnsupportedAgent(agent.to_string()))?;
        let label = parsed.id().to_string();
        let mut selected = self.agent.write().await;
        let changed = *selected != label;
        selected.clone_from(&label);
        drop(selected);
        if changed {
            self.publish_change();
        }
        Ok(label)
    }

    /// Every provider this daemon can drive, with its effective model,
    /// whether a key is already resolvable from the daemon's environment,
    /// and which one the next question goes to.
    pub async fn providers(&self) -> Vec<AskProviderInfo> {
        let selected = self.agent.read().await.clone();
        let providers = self.providers.read().await;
        provider_infos(&providers, &selected, |name| std::env::var(name).ok())
    }

    /// Apply `edit` under `[ask.providers.<id>]` and refresh the live
    /// settings from what was written. Each key is tri-state: absent leaves
    /// it alone, `Some(None)` removes it, `Some(Some(value))` sets it. The
    /// file is edited in place through `toml_edit`, validated as a full
    /// [`Config`] before anything touches disk, and swapped in atomically,
    /// so a bad value cannot leave the daemon unable to start.
    pub async fn configure_provider(
        &self,
        provider: &str,
        edit: AskProviderEdit,
    ) -> Result<Vec<AskProviderInfo>, AskError> {
        let parsed = AskProvider::parse(provider)
            .ok_or_else(|| AskError::UnsupportedAgent(provider.to_string()))?;
        let path = self
            .opts
            .config_path
            .clone()
            .ok_or_else(|| AskError::NoConfigPath(parsed.id().to_string()))?;
        let _guard = self.write_lock.lock().await;
        let config =
            write_provider_config(&path, parsed.id(), &edit.normalized()).map_err(AskError::Io)?;
        *self.providers.write().await = config.ask.providers;
        self.publish_change();
        Ok(self.providers().await)
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
    /// in the worker future and the child environment (or request header);
    /// persistence happens before it is moved into that future and contains
    /// no credential field.
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
        let Some(provider) = AskProvider::parse(&selected) else {
            return Err(AskError::UnsupportedAgent(selected));
        };
        let credential_key = take_credential(provider, credential)?;
        let provider_config = self
            .providers
            .read()
            .await
            .get(provider.id())
            .cloned()
            .unwrap_or_default();

        let write_guard = self.write_lock.lock().await;
        let conversation = self.ensure_active_conversation(provider.id()).await;
        let conversation_id = conversation.id.clone();
        if self.entries.read().await.iter().any(|entry| {
            entry.conversation_id.as_deref() == Some(conversation_id.as_str())
                && entry.status == AskStatus::Running
        }) {
            return Err(AskError::ConversationBusy);
        }
        let resume = conversation.agent_session_id.clone();
        // API providers remember nothing between calls; the store is their
        // thread. Read it before the new entry joins so the prompt being
        // asked is not replayed as history.
        let history = match provider.kind() {
            AskProviderKind::Api => replay_history(&self.entries.read().await, &conversation_id),
            AskProviderKind::Cli => Vec::new(),
        };
        let now = OffsetDateTime::now_utc();
        let entry = AskEntry {
            id: format!("ask_{:x}", next_id()),
            conversation_id: Some(conversation_id.clone()),
            prompt: prompt.to_string(),
            answer: String::new(),
            status: AskStatus::Running,
            agent: provider.id().to_string(),
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
            let api_key = resolve_api_key(provider, credential_key, &provider_config, |name| {
                std::env::var(name).ok()
            });
            let outcome = provider
                .run(Turn {
                    prompt: &prompt,
                    resume: resume.as_deref(),
                    history: &history,
                    cwd: &store.opts.cwd,
                    permission_mode: store.opts.permission_mode,
                    additional_dirs: &store.opts.additional_dirs,
                    timeout: Duration::from_secs(store.opts.timeout_secs.max(5)),
                    model: provider_config.model.as_deref(),
                    api_key: api_key.as_deref(),
                })
                .await;
            store.finish(&id, outcome).await;
        });

        Ok(entry)
    }

    /// One headless turn answered to the caller, using this store's
    /// provider settings (model, key lookup, cwd, timeout) but none of its
    /// history: nothing is recorded and no conversation advances. `agent`
    /// defaults to the selected one. This is what `work_compose` runs on —
    /// a drafting turn the user asked for by name, so `[ask].enabled` is
    /// not consulted, the same consent rule `muxa work init` follows.
    pub async fn one_shot_for(
        &self,
        agent: Option<&str>,
        prompt: &str,
        permission_mode: AskPermissionMode,
        credential: Option<AskCredential>,
    ) -> Result<AskAnswer, AskError> {
        let prompt = prompt.trim();
        if prompt.is_empty() {
            return Err(AskError::EmptyPrompt);
        }
        let selected = match agent {
            Some(name) => name.to_string(),
            None => self.agent.read().await.clone(),
        };
        let provider = AskProvider::parse(&selected)
            .ok_or_else(|| AskError::UnsupportedAgent(selected.clone()))?;
        let credential_key = take_credential(provider, credential)?;
        let provider_config = self
            .providers
            .read()
            .await
            .get(provider.id())
            .cloned()
            .unwrap_or_default();
        let api_key = resolve_api_key(provider, credential_key, &provider_config, |name| {
            std::env::var(name).ok()
        });
        provider
            .run(Turn {
                prompt,
                resume: None,
                history: &[],
                cwd: &self.opts.cwd,
                permission_mode,
                additional_dirs: &self.opts.additional_dirs,
                timeout: Duration::from_secs(self.opts.timeout_secs.max(5)),
                model: provider_config.model.as_deref(),
                api_key: api_key.as_deref(),
            })
            .await
            .map_err(AskError::Io)
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

/// A one-turn credential is only honoured for the provider it names; a key
/// for the wrong provider is refused before anything is spawned.
fn take_credential(
    provider: AskProvider,
    credential: Option<AskCredential>,
) -> Result<Option<String>, AskError> {
    match credential {
        Some(credential) if AskProvider::parse(&credential.agent) == Some(provider) => {
            Ok(Some(credential.api_key))
        }
        Some(credential) => Err(AskError::CredentialAgentMismatch {
            supplied: credential.agent,
            selected: provider.id().to_string(),
        }),
        None => Ok(None),
    }
}

/// One `ask_provider_configure` edit. Each key is tri-state so a client
/// can send only what it changed: `None` leaves the key as it is,
/// `Some(None)` removes it, `Some(Some(value))` sets it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AskProviderEdit {
    pub model: Option<Option<String>>,
    pub api_key_env: Option<Option<String>>,
}

impl AskProviderEdit {
    /// A blank value from a form field means "clear it", the same as
    /// `null`: an empty model or variable name could never be used.
    fn normalized(self) -> Self {
        let trim = |value: Option<Option<String>>| {
            value.map(|inner| {
                inner
                    .map(|value| value.trim().to_string())
                    .filter(|value| !value.is_empty())
            })
        };
        Self {
            model: trim(self.model),
            api_key_env: trim(self.api_key_env),
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

// ---------------------------------------------------------------------------
// Providers
// ---------------------------------------------------------------------------

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
    /// A provider id from [`supported_agents`].
    pub agent: &'a str,
    pub prompt: &'a str,
    pub cwd: &'a std::path::Path,
    pub permission_mode: AskPermissionMode,
    pub additional_dirs: &'a [PathBuf],
    pub timeout: Duration,
}

/// Providers this bridge can drive headlessly, in preference order.
///
/// Membership is not "muxa knows this agent" — the launcher knows more
/// (agy, opencode) — but "it has a print mode that reports completion as
/// a fact": an exit code plus a parseable envelope, or an HTTPS API with a
/// status code. Without that, reading an answer back means screen-scraping
/// a moving target.
#[must_use]
pub fn supported_agents() -> &'static [&'static str] {
    &["claude", "codex", "gemini", "anthropic", "openai"]
}

/// Run one headless turn and return its answer. The key for an API provider
/// comes from its environment variable; see [`one_shot_configured`] for the
/// `[ask.providers.<id>]` overrides.
///
/// # Errors
/// Returns [`AskError::UnsupportedAgent`] for an agent without a print
/// mode, [`AskError::EmptyPrompt`] for a blank prompt, and
/// [`AskError::Io`] when the child fails, times out, or answers with
/// something that is not a parseable result envelope.
pub async fn one_shot(request: OneShot<'_>) -> Result<AskAnswer, AskError> {
    one_shot_configured(request, None).await
}

/// [`one_shot`] with a provider's `[ask.providers.<id>]` settings: its
/// model, and the environment variable to fall back to for the key.
pub async fn one_shot_configured(
    request: OneShot<'_>,
    provider_config: Option<&AskProviderConfig>,
) -> Result<AskAnswer, AskError> {
    let prompt = request.prompt.trim();
    if prompt.is_empty() {
        return Err(AskError::EmptyPrompt);
    }
    let provider = AskProvider::parse(request.agent)
        .ok_or_else(|| AskError::UnsupportedAgent(request.agent.to_string()))?;
    let settings = provider_config.cloned().unwrap_or_default();
    let api_key = resolve_api_key(provider, None, &settings, |name| std::env::var(name).ok());
    provider
        .run(Turn {
            prompt,
            resume: None,
            history: &[],
            cwd: request.cwd,
            permission_mode: request.permission_mode,
            additional_dirs: request.additional_dirs,
            timeout: request.timeout.max(Duration::from_secs(5)),
            model: settings.model.as_deref(),
            api_key: api_key.as_deref(),
        })
        .await
        .map_err(AskError::Io)
}

/// How a provider is reached.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AskProviderKind {
    /// An agent CLI on `PATH`, driven in its print mode.
    Cli,
    /// An HTTPS API called directly.
    Api,
}

/// What a client needs to offer a provider: how to reach it, which
/// credential it takes, and the model it will use.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AskProviderInfo {
    pub id: String,
    pub title: String,
    pub kind: AskProviderKind,
    /// The CLI binary for `cli` providers; `null` for APIs.
    pub executable: Option<String>,
    /// Environment variable the provider's key is read from.
    pub credential_env: String,
    /// `false` for CLIs, which may be logged in already.
    pub credential_required: bool,
    /// `true` when the daemon can already resolve a key for this provider
    /// without one being sent: from `credential_env` in its own
    /// environment, or from the variable `[ask.providers.<id>]
    /// api_key_env` names. A client can then offer an API provider even
    /// with nothing stored on its side.
    pub credential_present: bool,
    /// The model a turn will use: configured, else the provider's default.
    /// `null` for a CLI with no configured model — it uses its own.
    pub model: Option<String>,
    /// Mirrors the store's current agent.
    pub selected: bool,
}

/// The provider list for `ask_providers`, in [`supported_agents`] order.
/// `env` reads the daemon's environment (injected so the list is testable
/// without touching the process environment).
#[must_use]
pub fn provider_infos(
    providers: &BTreeMap<String, AskProviderConfig>,
    selected: &str,
    env: impl Fn(&str) -> Option<String>,
) -> Vec<AskProviderInfo> {
    AskProvider::ALL
        .iter()
        .map(|provider| {
            let configured = providers.get(provider.id());
            let settings = configured.cloned().unwrap_or_default();
            AskProviderInfo {
                id: provider.id().to_string(),
                title: provider.title().to_string(),
                kind: provider.kind(),
                executable: provider.executable().map(str::to_string),
                credential_env: provider.credential_env().to_string(),
                credential_required: provider.credential_required(),
                credential_present: resolve_api_key(*provider, None, &settings, &env).is_some(),
                model: configured
                    .and_then(|config| config.model.clone())
                    .or_else(|| provider.default_model().map(str::to_string)),
                selected: provider.id() == selected,
            }
        })
        .collect()
}

/// A provider muxa can ask headlessly.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AskProvider {
    Claude,
    Codex,
    Gemini,
    Anthropic,
    OpenAi,
}

impl AskProvider {
    /// Every provider, in the order clients list them.
    pub const ALL: [Self; 5] = [
        Self::Claude,
        Self::Codex,
        Self::Gemini,
        Self::Anthropic,
        Self::OpenAi,
    ];

    #[must_use]
    pub fn parse(name: &str) -> Option<Self> {
        match name.trim().to_ascii_lowercase().as_str() {
            "claude" => Some(Self::Claude),
            "codex" => Some(Self::Codex),
            "gemini" => Some(Self::Gemini),
            "anthropic" => Some(Self::Anthropic),
            "openai" => Some(Self::OpenAi),
            _ => None,
        }
    }

    /// The stable id used on the wire and in config.
    #[must_use]
    pub fn id(self) -> &'static str {
        match self {
            Self::Claude => "claude",
            Self::Codex => "codex",
            Self::Gemini => "gemini",
            Self::Anthropic => "anthropic",
            Self::OpenAi => "openai",
        }
    }

    #[must_use]
    pub fn title(self) -> &'static str {
        match self {
            Self::Claude => "Claude Code",
            Self::Codex => "Codex CLI",
            Self::Gemini => "Gemini CLI",
            Self::Anthropic => "Anthropic API",
            Self::OpenAi => "OpenAI API",
        }
    }

    #[must_use]
    pub fn kind(self) -> AskProviderKind {
        match self {
            Self::Claude | Self::Codex | Self::Gemini => AskProviderKind::Cli,
            Self::Anthropic | Self::OpenAi => AskProviderKind::Api,
        }
    }

    /// The binary a CLI provider spawns.
    #[must_use]
    pub fn executable(self) -> Option<&'static str> {
        match self {
            Self::Claude => Some("claude"),
            Self::Codex => Some("codex"),
            Self::Gemini => Some("gemini"),
            Self::Anthropic | Self::OpenAi => None,
        }
    }

    /// The environment variable the provider's key is read from — and, for
    /// a one-turn credential, written to in the child's environment.
    #[must_use]
    pub fn credential_env(self) -> &'static str {
        match self {
            Self::Claude | Self::Anthropic => "ANTHROPIC_API_KEY",
            Self::Codex => "CODEX_API_KEY",
            Self::Gemini => "GEMINI_API_KEY",
            Self::OpenAi => "OPENAI_API_KEY",
        }
    }

    /// CLIs may be logged in; APIs never are.
    #[must_use]
    pub fn credential_required(self) -> bool {
        self.kind() == AskProviderKind::Api
    }

    /// The model an API provider uses when none is configured. CLIs pick
    /// their own.
    #[must_use]
    pub fn default_model(self) -> Option<&'static str> {
        match self {
            Self::Anthropic => Some("claude-sonnet-5"),
            Self::OpenAi => Some("gpt-5"),
            Self::Claude | Self::Codex | Self::Gemini => None,
        }
    }

    fn api_url(self) -> &'static str {
        match self {
            Self::Anthropic => ANTHROPIC_MESSAGES_URL,
            Self::OpenAi => OPENAI_CHAT_URL,
            Self::Claude | Self::Codex | Self::Gemini => "",
        }
    }

    /// Argv for one headless CLI turn. `resume` continues an existing
    /// conversation; `None` starts a new one. `model` is passed through
    /// when configured.
    fn argv(
        self,
        prompt: &str,
        resume: Option<&str>,
        permission_mode: AskPermissionMode,
        additional_dirs: &[PathBuf],
        model: Option<&str>,
    ) -> (&'static str, Vec<String>) {
        match self {
            Self::Claude => {
                let mut args = vec!["-p".to_string(), "--output-format".into(), "json".into()];
                match permission_mode {
                    AskPermissionMode::Default => {}
                    AskPermissionMode::Plan => args.push("--permission-mode=plan".into()),
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
                if let Some(model) = model {
                    args.push("--model".into());
                    args.push(model.to_string());
                }
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
                    AskPermissionMode::Plan => args.push("--sandbox=read-only".into()),
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
                if let Some(model) = model {
                    args.push("--model".into());
                    args.push(model.to_string());
                }
                args.push("--json".into());
                args.push(prompt.to_string());
                ("codex", args)
            }
            Self::Gemini => {
                // `-p` is gemini's headless mode; `--resume <session id>`
                // continues a session from the same project directory.
                let mut args = vec![
                    "-p".to_string(),
                    prompt.to_string(),
                    "--output-format".into(),
                    "json".into(),
                ];
                match permission_mode {
                    AskPermissionMode::Default => {}
                    AskPermissionMode::Plan => {
                        args.push("--approval-mode".into());
                        args.push("plan".into());
                    }
                    AskPermissionMode::Edit => {
                        args.push("--approval-mode".into());
                        args.push("auto_edit".into());
                    }
                    AskPermissionMode::Bypass => {
                        args.push("--approval-mode".into());
                        args.push("yolo".into());
                    }
                }
                for dir in additional_dirs {
                    args.push("--include-directories".into());
                    args.push(dir.display().to_string());
                }
                if let Some(model) = model {
                    args.push("--model".into());
                    args.push(model.to_string());
                }
                if let Some(id) = resume {
                    args.push("--resume".into());
                    args.push(id.to_string());
                }
                ("gemini", args)
            }
            Self::Anthropic | Self::OpenAi => ("", Vec::new()),
        }
    }

    async fn run(self, turn: Turn<'_>) -> Result<AskAnswer, String> {
        match self.kind() {
            AskProviderKind::Cli => self.run_cli(&turn).await,
            AskProviderKind::Api => self.call_api(self.api_url(), &turn).await,
        }
    }

    async fn run_cli(self, turn: &Turn<'_>) -> Result<AskAnswer, String> {
        let (bin, args) = self.argv(
            turn.prompt,
            turn.resume,
            turn.permission_mode,
            turn.additional_dirs,
            turn.model,
        );
        let mut cmd = tokio::process::Command::new(bin);
        cmd.args(&args)
            .current_dir(turn.cwd)
            .stdin(std::process::Stdio::null())
            .kill_on_drop(true);
        if let Some(api_key) = turn.api_key {
            cmd.env(self.credential_env(), api_key);
        }
        let output = tokio::time::timeout(turn.timeout, cmd.output())
            .await
            .map_err(|_| timeout_message(bin, turn.timeout))?
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
            Self::Gemini => parse_gemini_json(&stdout),
            Self::Anthropic | Self::OpenAi => unreachable!("API providers do not spawn"),
        }
    }

    /// One HTTPS turn against `url` (a parameter so tests can point it at a
    /// local server). The prior turns are replayed as `messages` ahead of
    /// the prompt; the answer is the assistant text, with no session id
    /// because there is nothing to resume.
    async fn call_api(self, url: &str, turn: &Turn<'_>) -> Result<AskAnswer, String> {
        let title = self.title();
        let Some(api_key) = turn.api_key else {
            return Err(format!(
                "no API key for {title}: pass one for this turn, set {env} in muxad's environment, \
                 or point [ask.providers.{id}] api_key_env at a variable that holds it",
                env = self.credential_env(),
                id = self.id(),
            ));
        };
        let model = turn
            .model
            .or_else(|| self.default_model())
            .unwrap_or_default();
        let messages = replay_messages(turn.history, turn.prompt);
        let client = reqwest::Client::builder()
            .timeout(turn.timeout)
            .build()
            .map_err(|e| format!("{title}: building the HTTP client: {e}"))?;
        let request = match self {
            Self::Anthropic => client
                .post(url)
                .header("x-api-key", api_key)
                .header("anthropic-version", ANTHROPIC_VERSION)
                .json(&anthropic_body(model, None, &messages)),
            Self::OpenAi => client
                .post(url)
                .bearer_auth(api_key)
                .json(&openai_body(model, &messages)),
            Self::Claude | Self::Codex | Self::Gemini => unreachable!("CLI providers spawn"),
        };
        let response = request.send().await.map_err(|e| {
            if e.is_timeout() {
                timeout_message(title, turn.timeout)
            } else {
                format!("{title}: {e}")
            }
        })?;
        let status = response.status().as_u16();
        let body = response.text().await.map_err(|e| {
            if e.is_timeout() {
                timeout_message(title, turn.timeout)
            } else {
                format!("{title}: reading the response: {e}")
            }
        })?;
        match self {
            Self::Anthropic => parse_anthropic_response(status, &body),
            Self::OpenAi => parse_openai_response(status, &body),
            Self::Claude | Self::Codex | Self::Gemini => unreachable!("CLI providers spawn"),
        }
    }
}

/// Everything one provider turn needs, resolved by the caller so the
/// provider itself reads no config and no environment.
struct Turn<'a> {
    prompt: &'a str,
    /// Provider session to continue (CLIs only).
    resume: Option<&'a str>,
    /// Prior turns of this conversation, oldest first (API providers only).
    history: &'a [ReplayTurn],
    cwd: &'a Path,
    permission_mode: AskPermissionMode,
    additional_dirs: &'a [PathBuf],
    timeout: Duration,
    model: Option<&'a str>,
    api_key: Option<&'a str>,
}

fn timeout_message(what: &str, timeout: Duration) -> String {
    format!(
        "{what} exceeded the ask timeout after {}s; it may still have been working — increase [ask].timeout_secs for long-running tasks",
        timeout.as_secs()
    )
}

/// Where an API key comes from, in order: the request's one-turn
/// credential, the provider's own environment variable in the daemon's
/// environment, then whatever variable `[ask.providers.<id>] api_key_env`
/// names. `env` is injected so the order is testable without touching the
/// process environment.
fn resolve_api_key(
    provider: AskProvider,
    credential: Option<String>,
    config: &AskProviderConfig,
    env: impl Fn(&str) -> Option<String>,
) -> Option<String> {
    credential
        .filter(|key| !key.trim().is_empty())
        .or_else(|| env(provider.credential_env()))
        .or_else(|| config.api_key_env.as_deref().and_then(&env))
        .filter(|key| !key.trim().is_empty())
}

/// One prior exchange, replayed to a provider that keeps no thread.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReplayTurn {
    pub prompt: String,
    pub answer: String,
}

/// The answered turns of `conversation_id`, oldest first, ready to replay.
#[must_use]
pub fn replay_history(entries: &[AskEntry], conversation_id: &str) -> Vec<ReplayTurn> {
    entries
        .iter()
        .filter(|entry| {
            entry.conversation_id.as_deref() == Some(conversation_id)
                && entry.status == AskStatus::Answered
        })
        .map(|entry| ReplayTurn {
            prompt: entry.prompt.clone(),
            answer: entry.answer.clone(),
        })
        .collect()
}

/// One chat message on the wire; both APIs share the `role`/`content`
/// pair.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ChatMessage {
    pub role: &'static str,
    pub content: String,
}

/// `history` trimmed to the replay budget — newest turns win, and the
/// budget counts whole turns so the thread never starts mid-exchange —
/// followed by the new prompt.
#[must_use]
pub fn replay_messages(history: &[ReplayTurn], prompt: &str) -> Vec<ChatMessage> {
    let mut kept = Vec::new();
    let mut chars = 0usize;
    for turn in history.iter().rev() {
        let size = turn.prompt.chars().count() + turn.answer.chars().count();
        if kept.len() >= REPLAY_MAX_TURNS || chars + size > REPLAY_MAX_CHARS {
            break;
        }
        chars += size;
        kept.push(turn);
    }
    let mut messages = Vec::with_capacity(kept.len() * 2 + 1);
    for turn in kept.into_iter().rev() {
        messages.push(ChatMessage {
            role: "user",
            content: turn.prompt.clone(),
        });
        messages.push(ChatMessage {
            role: "assistant",
            content: turn.answer.clone(),
        });
    }
    messages.push(ChatMessage {
        role: "user",
        content: prompt.to_string(),
    });
    messages
}

/// `POST /v1/messages` body: `{model, max_tokens, system?, messages}`.
#[must_use]
pub fn anthropic_body(
    model: &str,
    system: Option<&str>,
    messages: &[ChatMessage],
) -> serde_json::Value {
    let mut body = serde_json::json!({
        "model": model,
        "max_tokens": API_MAX_TOKENS,
        "messages": messages,
    });
    if let Some(system) = system {
        body["system"] = serde_json::Value::String(system.to_string());
    }
    body
}

/// `POST /v1/chat/completions` body: `{model, messages}`.
#[must_use]
pub fn openai_body(model: &str, messages: &[ChatMessage]) -> serde_json::Value {
    serde_json::json!({
        "model": model,
        "messages": messages,
    })
}

/// The `error.message` an API put in a failed response, or a trimmed
/// excerpt of the body when it did not send one.
fn api_error_detail(body: &str) -> String {
    serde_json::from_str::<serde_json::Value>(body)
        .ok()
        .and_then(|value| {
            value
                .get("error")
                .and_then(|error| error.get("message"))
                .and_then(serde_json::Value::as_str)
                .map(str::to_string)
        })
        .unwrap_or_else(|| {
            let excerpt: String = body.trim().chars().take(200).collect();
            if excerpt.is_empty() {
                "no error body".to_string()
            } else {
                excerpt
            }
        })
}

/// The Messages API answers with `content[]`; the text parts concatenated
/// are the answer. Cost is not reported.
pub fn parse_anthropic_response(status: u16, body: &str) -> Result<AskAnswer, String> {
    if !(200..300).contains(&status) {
        return Err(format!(
            "Anthropic API returned HTTP {status}: {}",
            api_error_detail(body)
        ));
    }
    let value: serde_json::Value =
        serde_json::from_str(body).map_err(|e| format!("parsing Anthropic API JSON: {e}"))?;
    let text: String = value
        .get("content")
        .and_then(serde_json::Value::as_array)
        .map(|parts| {
            parts
                .iter()
                .filter(|part| {
                    part.get("type")
                        .and_then(serde_json::Value::as_str)
                        .is_none_or(|kind| kind == "text")
                })
                .filter_map(|part| part.get("text").and_then(serde_json::Value::as_str))
                .collect()
        })
        .unwrap_or_default();
    if text.is_empty() {
        return Err("Anthropic API answered without text content".to_string());
    }
    Ok(AskAnswer {
        text,
        session_id: None,
        cost_usd: None,
    })
}

/// Chat Completions answers with `choices[0].message.content`, a string
/// or (for some models) an array of text parts.
pub fn parse_openai_response(status: u16, body: &str) -> Result<AskAnswer, String> {
    if !(200..300).contains(&status) {
        return Err(format!(
            "OpenAI API returned HTTP {status}: {}",
            api_error_detail(body)
        ));
    }
    let value: serde_json::Value =
        serde_json::from_str(body).map_err(|e| format!("parsing OpenAI API JSON: {e}"))?;
    let content = value
        .get("choices")
        .and_then(serde_json::Value::as_array)
        .and_then(|choices| choices.first())
        .and_then(|choice| choice.get("message"))
        .and_then(|message| message.get("content"));
    let text = match content {
        Some(serde_json::Value::String(text)) => text.clone(),
        Some(serde_json::Value::Array(parts)) => parts
            .iter()
            .filter_map(|part| part.get("text").and_then(serde_json::Value::as_str))
            .collect(),
        _ => String::new(),
    };
    if text.is_empty() {
        return Err("OpenAI API answered without message content".to_string());
    }
    Ok(AskAnswer {
        text,
        session_id: None,
        cost_usd: None,
    })
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

/// `gemini -p --output-format json` answers with one object:
/// `{"session_id", "response", "stats"}`, or `{"session_id", "error":
/// {"type", "message"}}` when the turn failed. Anything printed ahead of
/// the object (an extension banner, say) is skipped.
fn parse_gemini_json(stdout: &str) -> Result<AskAnswer, String> {
    let trimmed = stdout.trim();
    let candidate = match trimmed.find('{') {
        Some(0) | None => trimmed,
        Some(start) => &trimmed[start..],
    };
    let value: serde_json::Value =
        serde_json::from_str(candidate).map_err(|e| format!("parsing gemini JSON: {e}"))?;
    if let Some(error) = value.get("error") {
        let detail = error
            .get("message")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("gemini reported an error");
        return Err(detail.to_string());
    }
    let text = value
        .get("response")
        .and_then(serde_json::Value::as_str)
        .ok_or("gemini JSON has no response field")?
        .to_string();
    Ok(AskAnswer {
        text,
        session_id: value
            .get("session_id")
            .and_then(serde_json::Value::as_str)
            .map(str::to_string),
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

// ---------------------------------------------------------------------------
// [ask.providers.<id>] on disk
// ---------------------------------------------------------------------------

/// Apply `edit` to `[ask.providers.<id>]` in `path` — set, remove, or
/// leave each key — keeping every other byte of the file. Tables that end
/// up empty are dropped so a cleared provider leaves no stray header
/// behind. The merged text has to read back as a full [`Config`] before it
/// is written, and the write is tmp-then-rename with the file's existing
/// mode preserved. Returns the config as written.
pub fn write_provider_config(
    path: &Path,
    provider: &str,
    edit: &AskProviderEdit,
) -> Result<Config, String> {
    let mut document = match std::fs::read_to_string(path) {
        Ok(text) if !text.trim().is_empty() => text
            .parse::<toml_edit::DocumentMut>()
            .map_err(|e| format!("parsing {}: {e}", path.display()))?,
        Ok(_) => toml_edit::DocumentMut::new(),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => toml_edit::DocumentMut::new(),
        Err(error) => return Err(format!("reading {}: {error}", path.display())),
    };

    {
        let ask = implicit_table(document.as_table_mut(), "ask")?;
        let providers = implicit_table(ask, "providers")?;
        let entry = implicit_table(providers, provider)?;
        // A real header for the provider itself: `[ask.providers.<id>]`
        // is what the operator expects to find and edit.
        entry.set_implicit(false);
        for (key, change) in [
            ("model", edit.model.as_ref()),
            ("api_key_env", edit.api_key_env.as_ref()),
        ] {
            match change.map(Option::as_deref) {
                // Absent from the edit: leave the key exactly as it is.
                None => {}
                Some(Some(value)) => {
                    match entry.get_mut(key).and_then(toml_edit::Item::as_value_mut) {
                        // Replace in place so a comment on the line survives.
                        Some(existing) => {
                            let decor = existing.decor().clone();
                            *existing = toml_edit::Value::from(value);
                            *existing.decor_mut() = decor;
                        }
                        None => {
                            entry.insert(key, toml_edit::value(value));
                        }
                    }
                }
                Some(None) => {
                    entry.remove(key);
                }
            }
        }
        if entry.is_empty() {
            providers.remove(provider);
        }
        if providers.is_empty() {
            ask.remove("providers");
        }
        if ask.is_empty() && ask.is_implicit() {
            document.remove("ask");
        }
    }

    let text = document.to_string();
    let config: Config = toml::from_str(&text)
        .map_err(|e| format!("the updated config would not parse, so it was not written: {e}"))?;
    config
        .validate()
        .map_err(|e| format!("the updated config is invalid, so it was not written: {e}"))?;
    atomic_write(path, &text)?;
    Ok(config)
}

/// `table[key]` as a table, created implicit (header not rendered) when
/// absent. An existing non-table value is refused rather than clobbered.
fn implicit_table<'a>(
    table: &'a mut toml_edit::Table,
    key: &str,
) -> Result<&'a mut toml_edit::Table, String> {
    if table.get(key).is_none() {
        let mut fresh = toml_edit::Table::new();
        fresh.set_implicit(true);
        table.insert(key, toml_edit::Item::Table(fresh));
    }
    table
        .get_mut(key)
        .and_then(toml_edit::Item::as_table_mut)
        .ok_or_else(|| format!("`{key}` in config.toml is not a table"))
}

/// Write-then-rename in the target's directory, keeping the mode of the
/// file being replaced (a fresh file is owner-only, like the CLI's).
fn atomic_write(path: &Path, text: &str) -> Result<(), String> {
    use std::io::Write as _;
    if let Some(parent) = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
    {
        std::fs::create_dir_all(parent)
            .map_err(|e| format!("creating {}: {e}", parent.display()))?;
    }
    let permissions = match std::fs::metadata(path) {
        Ok(metadata) => Some(metadata.permissions()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
        Err(e) => return Err(format!("reading mode of {}: {e}", path.display())),
    };
    let tmp = path.with_extension(format!("toml.{}.tmp", std::process::id()));
    let result = (|| -> std::io::Result<()> {
        let mut file = std::fs::File::create(&tmp)?;
        file.write_all(text.as_bytes())?;
        match permissions {
            Some(permissions) => file.set_permissions(permissions)?,
            #[cfg(unix)]
            None => {
                use std::os::unix::fs::PermissionsExt;
                file.set_permissions(std::fs::Permissions::from_mode(0o600))?;
            }
            #[cfg(not(unix))]
            None => {}
        }
        file.sync_all()?;
        std::fs::rename(&tmp, path)
    })();
    if let Err(error) = result {
        let _ = std::fs::remove_file(&tmp);
        return Err(format!("writing {}: {error}", path.display()));
    }
    Ok(())
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
    fn gemini_json_yields_text_and_session_and_skips_a_banner() {
        // The shape `JsonFormatter.format` in gemini-cli 0.33 emits.
        let raw = concat!(
            "Loaded 2 extensions.\n",
            r#"{"session_id":"7d0e2f6c-1111-4d2c-9d1e-000000000000","response":"PONG","stats":{"models":{}}}"#,
        );
        let answer = parse_gemini_json(raw).unwrap();
        assert_eq!(answer.text, "PONG");
        assert_eq!(
            answer.session_id.as_deref(),
            Some("7d0e2f6c-1111-4d2c-9d1e-000000000000")
        );
        assert_eq!(answer.cost_usd, None);
    }

    #[test]
    fn gemini_error_object_turns_into_an_error() {
        let raw = r#"{"session_id":"s","error":{"type":"FatalAuthenticationError","message":"no key","code":41}}"#;
        assert_eq!(parse_gemini_json(raw).unwrap_err(), "no key");
        assert!(parse_gemini_json("not json").is_err());
    }

    #[test]
    fn claude_argv_only_resumes_when_there_is_a_thread() {
        let (bin, fresh) =
            AskProvider::Claude.argv("hi", None, AskPermissionMode::Default, &[], None);
        assert_eq!(bin, "claude");
        assert!(!fresh.contains(&"--resume".to_string()));
        assert_eq!(fresh.last().unwrap(), "hi");

        let (_, resumed) =
            AskProvider::Claude.argv("hi", Some("s-9"), AskPermissionMode::Default, &[], None);
        let at = resumed.iter().position(|a| a == "--resume").unwrap();
        assert_eq!(resumed[at + 1], "s-9");
    }

    #[test]
    fn execution_controls_are_explicit_in_agent_argv() {
        let dirs = [PathBuf::from("/nfs/home/june")];
        let (_, claude) =
            AskProvider::Claude.argv("resolve", None, AskPermissionMode::Bypass, &dirs, None);
        assert!(claude.contains(&"--dangerously-skip-permissions".to_string()));
        assert!(claude.contains(&"--add-dir=/nfs/home/june".to_string()));

        let (_, codex) =
            AskProvider::Codex.argv("resolve", None, AskPermissionMode::Bypass, &dirs, None);
        assert!(codex.contains(&"--dangerously-bypass-approvals-and-sandbox".to_string()));
        assert!(codex.contains(&"--add-dir=/nfs/home/june".to_string()));

        let (_, safe) =
            AskProvider::Claude.argv("question", None, AskPermissionMode::Default, &dirs, None);
        assert!(!safe.contains(&"--dangerously-skip-permissions".to_string()));
    }

    #[test]
    fn plan_mode_is_read_only_in_every_cli_argv() {
        let (_, claude) =
            AskProvider::Claude.argv("draft", None, AskPermissionMode::Plan, &[], None);
        assert!(claude.contains(&"--permission-mode=plan".to_string()));
        assert!(!claude.contains(&"--dangerously-skip-permissions".to_string()));

        let (_, codex) = AskProvider::Codex.argv("draft", None, AskPermissionMode::Plan, &[], None);
        assert!(codex.contains(&"--sandbox=read-only".to_string()));
        assert!(!codex.iter().any(|arg| arg.contains("bypass")));

        let (_, gemini) =
            AskProvider::Gemini.argv("draft", None, AskPermissionMode::Plan, &[], None);
        let at = gemini.iter().position(|a| a == "--approval-mode").unwrap();
        assert_eq!(gemini[at + 1], "plan");
        assert!(!gemini.contains(&"--yolo".to_string()));
    }

    #[test]
    fn gemini_argv_is_headless_json_with_directories_model_and_resume() {
        let dirs = [PathBuf::from("/srv/shared")];
        let (bin, args) = AskProvider::Gemini.argv(
            "hi",
            Some("sess-1"),
            AskPermissionMode::Bypass,
            &dirs,
            Some("gemini-2.5-pro"),
        );
        assert_eq!(bin, "gemini");
        assert_eq!(&args[..4], ["-p", "hi", "--output-format", "json"]);
        let mode = args.iter().position(|a| a == "--approval-mode").unwrap();
        assert_eq!(args[mode + 1], "yolo");
        let dir = args
            .iter()
            .position(|a| a == "--include-directories")
            .unwrap();
        assert_eq!(args[dir + 1], "/srv/shared");
        let model = args.iter().position(|a| a == "--model").unwrap();
        assert_eq!(args[model + 1], "gemini-2.5-pro");
        let resume = args.iter().position(|a| a == "--resume").unwrap();
        assert_eq!(args[resume + 1], "sess-1");
    }

    #[test]
    fn a_configured_model_reaches_claude_and_codex_argv() {
        let (_, claude) = AskProvider::Claude.argv(
            "hi",
            None,
            AskPermissionMode::Default,
            &[],
            Some("claude-opus-5"),
        );
        let at = claude.iter().position(|a| a == "--model").unwrap();
        assert_eq!(claude[at + 1], "claude-opus-5");
        let (_, codex) =
            AskProvider::Codex.argv("hi", None, AskPermissionMode::Default, &[], Some("gpt-5"));
        let at = codex.iter().position(|a| a == "--model").unwrap();
        assert_eq!(codex[at + 1], "gpt-5");
        // And the prompt is still the trailing argument for both.
        assert_eq!(claude.last().unwrap(), "hi");
        assert_eq!(codex.last().unwrap(), "hi");
    }

    #[test]
    fn unattended_ask_defaults_support_resolver_workflows() {
        assert_eq!(
            AskOptions::default().permission_mode,
            AskPermissionMode::Bypass
        );
        assert_eq!(AskOptions::default().timeout_secs, DEFAULT_ASK_TIMEOUT_SECS);
    }

    #[test]
    fn every_provider_id_round_trips_in_the_documented_order() {
        assert_eq!(
            supported_agents(),
            &["claude", "codex", "gemini", "anthropic", "openai"]
        );
        for (provider, id) in AskProvider::ALL.iter().zip(supported_agents()) {
            assert_eq!(provider.id(), *id);
            assert_eq!(AskProvider::parse(id), Some(*provider));
            assert_eq!(AskProvider::parse(&id.to_uppercase()), Some(*provider));
        }
        assert_eq!(AskProvider::parse("bard"), None);
        assert_eq!(AskProvider::Claude.kind(), AskProviderKind::Cli);
        assert_eq!(AskProvider::OpenAi.kind(), AskProviderKind::Api);
        assert_eq!(AskProvider::Gemini.credential_env(), "GEMINI_API_KEY");
        assert_eq!(AskProvider::Anthropic.credential_env(), "ANTHROPIC_API_KEY");
        assert_eq!(AskProvider::OpenAi.credential_env(), "OPENAI_API_KEY");
    }

    #[test]
    fn provider_infos_carry_effective_models_and_the_selection() {
        let mut providers = BTreeMap::new();
        providers.insert(
            "openai".to_string(),
            AskProviderConfig {
                model: Some("gpt-5-mini".into()),
                api_key_env: Some("WORK_OPENAI".into()),
            },
        );
        providers.insert(
            "codex".to_string(),
            AskProviderConfig {
                model: Some("gpt-5-codex".into()),
                api_key_env: None,
            },
        );
        // The daemon's environment holds a Gemini key and the variable the
        // openai override names; nothing for anthropic.
        let env = |name: &str| match name {
            "GEMINI_API_KEY" => Some("g".to_string()),
            "WORK_OPENAI" => Some("o".to_string()),
            _ => None,
        };
        let infos = provider_infos(&providers, "anthropic", env);
        let ids: Vec<&str> = infos.iter().map(|info| info.id.as_str()).collect();
        assert_eq!(ids, supported_agents());

        let anthropic = &infos[3];
        assert_eq!(
            serde_json::to_value(anthropic).unwrap(),
            serde_json::json!({
                "id": "anthropic", "title": "Anthropic API", "kind": "api",
                "executable": null, "credential_env": "ANTHROPIC_API_KEY",
                "credential_required": true, "credential_present": false,
                "model": "claude-sonnet-5", "selected": true,
            })
        );
        let openai = &infos[4];
        assert_eq!(openai.model.as_deref(), Some("gpt-5-mini"));
        assert!(openai.credential_present, "resolved through api_key_env");
        assert!(!openai.selected);
        let claude = &infos[0];
        assert_eq!(claude.kind, AskProviderKind::Cli);
        assert_eq!(claude.executable.as_deref(), Some("claude"));
        assert!(!claude.credential_required);
        assert!(!claude.credential_present);
        assert_eq!(claude.model, None);
        assert!(infos[2].credential_present, "GEMINI_API_KEY is set");
        assert_eq!(infos[1].model.as_deref(), Some("gpt-5-codex"));
        // The list survives a JSON round trip for native clients.
        let text = serde_json::to_string(&infos).unwrap();
        let back: Vec<AskProviderInfo> = serde_json::from_str(&text).unwrap();
        assert_eq!(back, infos);
    }

    #[test]
    fn api_key_resolution_prefers_the_request_then_env_then_configured_variable() {
        let config = AskProviderConfig {
            model: None,
            api_key_env: Some("WORK_KEY".into()),
        };
        let env = |name: &str| match name {
            "OPENAI_API_KEY" => Some("from-env".to_string()),
            "WORK_KEY" => Some("from-work".to_string()),
            _ => None,
        };
        assert_eq!(
            resolve_api_key(
                AskProvider::OpenAi,
                Some("from-request".into()),
                &config,
                env
            )
            .as_deref(),
            Some("from-request")
        );
        assert_eq!(
            resolve_api_key(AskProvider::OpenAi, None, &config, env).as_deref(),
            Some("from-env")
        );
        let only_work = |name: &str| (name == "WORK_KEY").then(|| "from-work".to_string());
        assert_eq!(
            resolve_api_key(AskProvider::OpenAi, None, &config, only_work).as_deref(),
            Some("from-work")
        );
        assert_eq!(
            resolve_api_key(AskProvider::OpenAi, Some("  ".into()), &config, |_| None),
            None
        );
    }

    fn turn(index: usize, size: usize) -> ReplayTurn {
        ReplayTurn {
            prompt: format!("q{index}"),
            answer: "a".repeat(size),
        }
    }

    #[test]
    fn replay_keeps_the_most_recent_turns_within_the_budget() {
        // 50 short turns: the last 40 survive, oldest first, then the prompt.
        let history: Vec<ReplayTurn> = (0..50).map(|i| turn(i, 3)).collect();
        let messages = replay_messages(&history, "now");
        assert_eq!(messages.len(), REPLAY_MAX_TURNS * 2 + 1);
        assert_eq!(messages[0].role, "user");
        assert_eq!(messages[0].content, "q10");
        assert_eq!(messages[1].role, "assistant");
        assert_eq!(messages[messages.len() - 2].content, "a".repeat(3));
        assert_eq!(messages.last().unwrap().role, "user");
        assert_eq!(messages.last().unwrap().content, "now");

        // Character budget: three 25k-character turns keep only the newest
        // two, and never a half turn.
        let big: Vec<ReplayTurn> = (0..3).map(|i| turn(i, 25_000)).collect();
        let messages = replay_messages(&big, "now");
        assert_eq!(messages.len(), 5);
        assert_eq!(messages[0].content, "q1");

        // No history is just the prompt.
        let messages = replay_messages(&[], "hello");
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0].content, "hello");
    }

    #[test]
    fn replay_history_takes_only_answered_turns_of_the_conversation() {
        let now = OffsetDateTime::now_utc();
        let entry = |id: &str, conversation: &str, status: AskStatus| AskEntry {
            id: id.into(),
            conversation_id: Some(conversation.into()),
            prompt: format!("prompt {id}"),
            answer: format!("answer {id}"),
            status,
            agent: "anthropic".into(),
            agent_session_id: None,
            cwd: "/tmp".into(),
            asked_at: now,
            answered_at: Some(now),
            cost_usd: None,
            error: None,
        };
        let entries = vec![
            entry("1", "c1", AskStatus::Answered),
            entry("2", "c2", AskStatus::Answered),
            entry("3", "c1", AskStatus::Failed),
            entry("4", "c1", AskStatus::Answered),
        ];
        let history = replay_history(&entries, "c1");
        assert_eq!(
            history,
            vec![
                ReplayTurn {
                    prompt: "prompt 1".into(),
                    answer: "answer 1".into()
                },
                ReplayTurn {
                    prompt: "prompt 4".into(),
                    answer: "answer 4".into()
                },
            ]
        );
    }

    #[test]
    fn api_bodies_have_the_documented_shape() {
        let messages = replay_messages(
            &[ReplayTurn {
                prompt: "earlier".into(),
                answer: "reply".into(),
            }],
            "now",
        );
        let anthropic = anthropic_body("claude-sonnet-5", None, &messages);
        assert_eq!(
            anthropic,
            serde_json::json!({
                "model": "claude-sonnet-5",
                "max_tokens": API_MAX_TOKENS,
                "messages": [
                    {"role": "user", "content": "earlier"},
                    {"role": "assistant", "content": "reply"},
                    {"role": "user", "content": "now"},
                ],
            })
        );
        let with_system = anthropic_body("m", Some("be brief"), &messages);
        assert_eq!(with_system["system"], "be brief");

        let openai = openai_body("gpt-5", &messages);
        assert_eq!(
            openai,
            serde_json::json!({
                "model": "gpt-5",
                "messages": [
                    {"role": "user", "content": "earlier"},
                    {"role": "assistant", "content": "reply"},
                    {"role": "user", "content": "now"},
                ],
            })
        );
    }

    #[test]
    fn anthropic_responses_concatenate_text_and_surface_api_errors() {
        let ok = r#"{"id":"msg_1","type":"message","role":"assistant","model":"claude-sonnet-5",
            "content":[{"type":"text","text":"Hello, "},{"type":"tool_use","id":"x","name":"n","input":{}},{"type":"text","text":"world"}],
            "stop_reason":"end_turn","usage":{"input_tokens":3,"output_tokens":2}}"#;
        let answer = parse_anthropic_response(200, ok).unwrap();
        assert_eq!(answer.text, "Hello, world");
        assert_eq!(answer.session_id, None);
        assert_eq!(answer.cost_usd, None);

        let failed = r#"{"type":"error","error":{"type":"authentication_error","message":"invalid x-api-key"}}"#;
        let error = parse_anthropic_response(401, failed).unwrap_err();
        assert_eq!(error, "Anthropic API returned HTTP 401: invalid x-api-key");

        let html = parse_anthropic_response(502, "<html>bad gateway</html>").unwrap_err();
        assert!(
            html.starts_with("Anthropic API returned HTTP 502: <html>"),
            "{html}"
        );
        assert!(parse_anthropic_response(200, r#"{"content":[]}"#).is_err());
    }

    #[test]
    fn openai_responses_take_the_first_choice_and_surface_api_errors() {
        let ok = r#"{"id":"chatcmpl-1","object":"chat.completion","choices":[
            {"index":0,"message":{"role":"assistant","content":"Hi there"},"finish_reason":"stop"},
            {"index":1,"message":{"role":"assistant","content":"ignored"},"finish_reason":"stop"}],
            "usage":{"prompt_tokens":1,"completion_tokens":2}}"#;
        let answer = parse_openai_response(200, ok).unwrap();
        assert_eq!(answer.text, "Hi there");
        assert_eq!(answer.session_id, None);

        let parts = r#"{"choices":[{"message":{"role":"assistant","content":[{"type":"text","text":"a"},{"type":"text","text":"b"}]}}]}"#;
        assert_eq!(parse_openai_response(200, parts).unwrap().text, "ab");

        let failed = r#"{"error":{"message":"Incorrect API key provided","type":"invalid_request_error","code":"invalid_api_key"}}"#;
        assert_eq!(
            parse_openai_response(401, failed).unwrap_err(),
            "OpenAI API returned HTTP 401: Incorrect API key provided"
        );
        assert!(parse_openai_response(200, r#"{"choices":[]}"#).is_err());
    }

    #[tokio::test]
    async fn an_api_turn_posts_the_replayed_thread_and_reads_the_answer() {
        use wiremock::matchers::{body_partial_json, header, method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/messages"))
            .and(header("x-api-key", "sk-test"))
            .and(header("anthropic-version", ANTHROPIC_VERSION))
            .and(body_partial_json(serde_json::json!({
                "model": "claude-opus-5",
                "messages": [
                    {"role": "user", "content": "first"},
                    {"role": "assistant", "content": "one"},
                    {"role": "user", "content": "second"},
                ],
            })))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "content": [{"type": "text", "text": "two"}],
            })))
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/v1/chat/completions"))
            .and(header("authorization", "Bearer sk-open"))
            .respond_with(ResponseTemplate::new(429).set_body_json(serde_json::json!({
                "error": {"message": "slow down", "type": "rate_limit"},
            })))
            .mount(&server)
            .await;

        let history = vec![ReplayTurn {
            prompt: "first".into(),
            answer: "one".into(),
        }];
        let cwd = std::env::temp_dir();
        let turn = |api_key: &'static str, model: Option<&'static str>| Turn {
            prompt: "second",
            resume: None,
            history: &history,
            cwd: &cwd,
            permission_mode: AskPermissionMode::Plan,
            additional_dirs: &[],
            timeout: Duration::from_secs(5),
            model,
            api_key: Some(api_key),
        };
        let answer = AskProvider::Anthropic
            .call_api(
                &format!("{}/v1/messages", server.uri()),
                &turn("sk-test", Some("claude-opus-5")),
            )
            .await
            .unwrap();
        assert_eq!(answer.text, "two");
        assert_eq!(answer.session_id, None);

        let error = AskProvider::OpenAi
            .call_api(
                &format!("{}/v1/chat/completions", server.uri()),
                &turn("sk-open", None),
            )
            .await
            .unwrap_err();
        assert_eq!(error, "OpenAI API returned HTTP 429: slow down");

        // No key at all fails before any request leaves the process.
        let missing = AskProvider::OpenAi
            .call_api(
                &format!("{}/v1/chat/completions", server.uri()),
                &Turn {
                    api_key: None,
                    ..turn("", None)
                },
            )
            .await
            .unwrap_err();
        assert!(missing.contains("no API key for OpenAI API"), "{missing}");
        assert!(missing.contains("OPENAI_API_KEY"), "{missing}");
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
    async fn every_provider_can_be_selected_and_listed() {
        let store = AskStore::in_memory(AskOptions::default());
        for id in supported_agents() {
            assert_eq!(store.set_agent(id).await.unwrap(), *id);
            let infos = store.providers().await;
            let selected: Vec<&str> = infos
                .iter()
                .filter(|info| info.selected)
                .map(|info| info.id.as_str())
                .collect();
            assert_eq!(selected, vec![*id]);
        }
        assert_eq!(store.set_agent("OpenAI").await.unwrap(), "openai");
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
        let error = store.set_agent("bard").await.unwrap_err().to_string();
        assert!(error.contains("is not supported"), "{error}");
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

        // The same rule guards the drafting turn.
        let result = store
            .one_shot_for(
                Some("anthropic"),
                "draft",
                AskPermissionMode::Plan,
                Some(AskCredential {
                    agent: "openai".into(),
                    api_key: "secret".into(),
                }),
            )
            .await;
        assert!(matches!(
            result,
            Err(AskError::CredentialAgentMismatch { .. })
        ));
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
        assert!(matches!(
            store
                .one_shot_for(None, " ", AskPermissionMode::Plan, None)
                .await,
            Err(AskError::EmptyPrompt)
        ));
    }

    #[test]
    fn provider_config_is_written_in_place_and_cleared_without_residue() {
        let directory = tempdir().unwrap();
        let path = directory.path().join("config.toml");
        std::fs::write(
            &path,
            "# operator notes\n[watch]\ntheme = \"classic\"\n\n[ask]\nenabled = true # keep\n",
        )
        .unwrap();

        let config = write_provider_config(
            &path,
            "anthropic",
            &AskProviderEdit {
                model: Some(Some("claude-opus-5".into())),
                api_key_env: Some(Some("WORK_KEY".into())),
            },
        )
        .unwrap();
        assert_eq!(
            config.ask.providers["anthropic"].model.as_deref(),
            Some("claude-opus-5")
        );
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(
            text.starts_with("# operator notes\n[watch]\ntheme = \"classic\"\n"),
            "{text}"
        );
        assert!(text.contains("enabled = true # keep"), "{text}");
        assert!(text.contains("[ask.providers.anthropic]\n"), "{text}");
        assert!(text.contains("model = \"claude-opus-5\""), "{text}");
        assert!(text.contains("api_key_env = \"WORK_KEY\""), "{text}");
        assert!(!text.contains("[ask.providers]\n"), "{text}");
        assert!(config.ask.enabled);

        // An edit that names only `model` leaves `api_key_env` alone…
        let config = write_provider_config(
            &path,
            "anthropic",
            &AskProviderEdit {
                model: Some(Some("claude-sonnet-5".into())),
                api_key_env: None,
            },
        )
        .unwrap();
        assert_eq!(
            config.ask.providers["anthropic"].api_key_env.as_deref(),
            Some("WORK_KEY")
        );
        // …an empty edit changes nothing…
        let before = std::fs::read_to_string(&path).unwrap();
        write_provider_config(&path, "anthropic", &AskProviderEdit::default()).unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), before);
        // …clearing one key keeps the other, and clearing both drops the table.
        let config = write_provider_config(
            &path,
            "anthropic",
            &AskProviderEdit {
                model: Some(None),
                api_key_env: None,
            },
        )
        .unwrap();
        assert_eq!(config.ask.providers["anthropic"].model, None);
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(!text.contains("model ="), "{text}");
        assert!(text.contains("api_key_env = \"WORK_KEY\""), "{text}");

        let config = write_provider_config(
            &path,
            "anthropic",
            &AskProviderEdit {
                model: Some(None),
                api_key_env: Some(None),
            },
        )
        .unwrap();
        assert!(config.ask.providers.is_empty());
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(!text.contains("providers"), "{text}");
        assert!(text.contains("[ask]\nenabled = true # keep"), "{text}");
        assert!(Config::load(&path).is_ok());
    }

    #[test]
    fn provider_config_creates_a_missing_file_with_only_the_provider_header() {
        let directory = tempdir().unwrap();
        let path = directory.path().join("nested").join("config.toml");
        write_provider_config(
            &path,
            "openai",
            &AskProviderEdit {
                model: Some(Some("gpt-5-mini".into())),
                api_key_env: None,
            },
        )
        .unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        assert_eq!(text, "[ask.providers.openai]\nmodel = \"gpt-5-mini\"\n");
        assert!(Config::load(&path).is_ok());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600, "{mode:o}");
        }
    }

    #[test]
    fn provider_config_refuses_a_broken_file_and_keeps_it() {
        let directory = tempdir().unwrap();
        let path = directory.path().join("config.toml");
        std::fs::write(&path, "[watch\n").unwrap();
        let error = write_provider_config(
            &path,
            "openai",
            &AskProviderEdit {
                model: Some(Some("gpt-5".into())),
                api_key_env: None,
            },
        )
        .unwrap_err();
        assert!(error.contains("parsing"), "{error}");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "[watch\n");
    }

    #[tokio::test]
    async fn configuring_a_provider_updates_the_live_list_and_refuses_unknowns() {
        let directory = tempdir().unwrap();
        let path = directory.path().join("config.toml");
        let store = AskStore::in_memory(AskOptions {
            config_path: Some(path.clone()),
            ..AskOptions::default()
        });
        let updates = store.subscribe();
        let infos = store
            .configure_provider(
                "openai",
                AskProviderEdit {
                    model: Some(Some("gpt-5-mini".into())),
                    api_key_env: Some(Some(" WORK_OPENAI ".into())),
                },
            )
            .await
            .unwrap();
        let openai = infos.iter().find(|info| info.id == "openai").unwrap();
        assert_eq!(openai.model.as_deref(), Some("gpt-5-mini"));
        assert!(updates.has_changed().unwrap());
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains("api_key_env = \"WORK_OPENAI\""), "{text}");

        // Blank clears, like null; an absent key stays.
        let infos = store
            .configure_provider(
                "openai",
                AskProviderEdit {
                    model: Some(Some(String::new())),
                    api_key_env: None,
                },
            )
            .await
            .unwrap();
        let openai = infos.iter().find(|info| info.id == "openai").unwrap();
        assert_eq!(openai.model.as_deref(), Some("gpt-5"));
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains("api_key_env = \"WORK_OPENAI\""), "{text}");
        store
            .configure_provider(
                "openai",
                AskProviderEdit {
                    model: None,
                    api_key_env: Some(None),
                },
            )
            .await
            .unwrap();
        assert!(!std::fs::read_to_string(&path)
            .unwrap()
            .contains("providers"));

        assert!(matches!(
            store
                .configure_provider("bard", AskProviderEdit::default())
                .await,
            Err(AskError::UnsupportedAgent(_))
        ));
        let pathless = AskStore::in_memory(AskOptions::default());
        assert!(matches!(
            pathless
                .configure_provider("openai", AskProviderEdit::default())
                .await,
            Err(AskError::NoConfigPath(_))
        ));
    }
}
