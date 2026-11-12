//! **Internal:** `#[doc(hidden)]`, the account module's interface to core.
//!
//! Account swapping is an optional module (`tend-aas`): choosing accounts,
//! stopping, relaunching and resuming agents, swap jobs, and the `aas`
//! integration live there. Core only needs to *speak* about swaps — parse
//! their `[swap]` configuration, carry requests and job views over IPC, and
//! answer "not available" when no module is attached — so those types live
//! here, with no swapping code behind them.
//!
//! The daemon attaches a module by handing the IPC server an
//! `Arc<dyn SwapJobs>`; without one every request is refused with the
//! reason [`UnopenedSwap`] was built with.

use crate::event::AgentKind;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// `[modules]` in `config.toml`: optional modules this install uses.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ModulesConfig {
    /// The account module: `tend swap`, the `swap_account` automation
    /// action, `tend_swap_account`, and `[agent.<program>].account`
    /// launches. Unset: on when this config already uses accounts
    /// ([`crate::Config::accounts_module`]).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub accounts: Option<bool>,
}

impl ModulesConfig {
    #[must_use]
    pub fn is_unset(&self) -> bool {
        self.accounts.is_none()
    }
}

/// Whether the account module is on for a config, and on what evidence.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModuleSwitch {
    pub enabled: bool,
    /// The settings that turned it on when `[modules] accounts` is unset.
    pub uses: Vec<String>,
    /// `[modules] accounts` was set.
    pub explicit: bool,
}

/// Why the account module does not answer, for a build that carries it
/// (`compiled`) or one that does not.
#[must_use]
pub fn module_unavailable(compiled: bool) -> String {
    if compiled {
        "the account module is not enabled: set `[modules] accounts = true` in config.toml \
         (or configure `[agent.<program>].account` / `[[accounts.<provider>]]`) and restart tendd"
            .into()
    } else {
        "the account module is not in this build: rebuild tend-cli and tendd with the `aas` \
         feature (on by default)"
            .into()
    }
}

/// `[swap]` in `config.toml`: defaults shared by `tend swap` and the
/// `swap_account` automation action.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct SwapConfig {
    /// What to do with an agent started without `aas` (`claude` / `codex`
    /// run bare).
    pub bare: BareMode,
    /// Prompt sent after the resume so the interrupted turn continues.
    /// Empty disables it.
    pub continue_text: String,
    /// Which swapped agents get [`Self::continue_text`].
    pub continue_when: ContinueWhen,
    /// What a relaunch must show before the follow-up is typed into it.
    pub continue_evidence: ContinueEvidence,
    /// The `aas` binary, an optional account backend used when a provider
    /// lists no `[[accounts.<provider>]]`. The daemon may not inherit the
    /// login shell's `PATH`, so a bare `aas` is also looked for in the
    /// usual install locations; when it is not found, swaps have no
    /// account to move to and launches start bare.
    pub aas: String,
}

impl Default for SwapConfig {
    fn default() -> Self {
        Self {
            bare: BareMode::Skip,
            continue_text: "continue".into(),
            continue_when: ContinueWhen::Limited,
            continue_evidence: ContinueEvidence::Binding,
            aas: "aas".into(),
        }
    }
}

/// An agent launched without the `aas` wrapper.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, strum::Display)]
#[serde(rename_all = "snake_case")]
#[strum(serialize_all = "snake_case")]
pub enum BareMode {
    /// Leave it alone.
    Skip,
    /// Relaunch it as `aas e <account> …`.
    Wrap,
}

/// Which swapped agents get the follow-up prompt.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, strum::Display)]
#[serde(rename_all = "snake_case")]
#[strum(serialize_all = "snake_case")]
pub enum ContinueWhen {
    /// Only agents whose turn was cut off: stopped by a usage cap, or
    /// interrupted mid-turn by the swap itself (`--force`).
    Limited,
    Always,
    Never,
}

/// The evidence a relaunched agent must show before a follow-up is typed
/// into it. Either way the pane's half-typed draft is only put back into a
/// relaunch that shows the resume binding.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, strum::Display)]
#[serde(rename_all = "snake_case")]
#[strum(serialize_all = "snake_case")]
pub enum ContinueEvidence {
    /// The relaunched process's own arguments resume exactly the planned
    /// conversation.
    #[default]
    Binding,
    /// That, and the conversation was read from the stopped process itself,
    /// and the relaunch was confirmed on the assigned account.
    Verified,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, strum::Display)]
#[serde(rename_all = "snake_case")]
#[strum(serialize_all = "snake_case")]
pub enum SwapStatus {
    /// Relaunched on the new account.
    Swapped,
    /// `dry_run`: this is what would have been done.
    Planned,
    /// Deliberately left alone.
    Skipped,
    /// Tried, and did not get as far as a running agent.
    Failed,
}

/// One agent's result.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SwapOutcome {
    pub pane: Option<String>,
    pub agent_session_id: String,
    pub kind: AgentKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub from: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub to: Option<String>,
    pub status: SwapStatus,
    /// The line typed (or that would be typed) into the pane.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub command: Option<String>,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub note: String,
    /// Why this agent was planned a follow-up — its eligibility under the
    /// policy, set when the plan is made. Not evidence that the prompt was
    /// sent or that the agent acted on it; a failed swap keeps it too.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub continue_reason: Option<ContinueReason>,
    /// Where the resumed conversation id came from.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id_source: Option<SessionIdSource>,
    /// Whether the running process itself was read to be on that
    /// conversation (Claude's session file on Linux). `false` means the id
    /// was taken on the registry's or command line's word.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub conversation_verified: Option<bool>,
    /// Why it could not be verified: an unsupported provider or platform,
    /// or no file naming the process.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub conversation_unverified_reason: Option<String>,
    /// `false` for a row tend only saw on screen, never from the agent's
    /// own hooks: its state is a guess.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub registered: Option<bool>,
    /// Why a follow-up that was due was not typed (`binding_missing`,
    /// `unverified_conversation`, …): the relaunch did not show the
    /// evidence `continue_evidence` asks for.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub continue_withheld: Option<String>,
    /// A typed follow-up: what the transport said, and what followed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub follow_up: Option<FollowUpReport>,
}

/// A typed follow-up as reported: codes, never its text.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FollowUpReport {
    /// Sent to a session kept on its original account.
    #[serde(default)]
    pub kept: bool,
    /// `delivered`, `refused` or `unconfirmed`: the transport's word only.
    pub submission: String,
    /// `prompt_hook`, `screen_activity` or `none_seen`; absent while (or if
    /// never) the wait for it ended.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub evidence: Option<String>,
}

/// The evidence a swap resumed a conversation by.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, strum::Display)]
#[serde(rename_all = "snake_case")]
#[strum(serialize_all = "snake_case")]
pub enum SessionIdSource {
    /// The agent's own hooks, confirmed by the running process where that
    /// can be checked.
    Registry,
    /// `--resume <id>` / `resume <id>` on the agent's command line.
    Argv,
    /// Claude's `sessions/<pid>.json` for this exact process.
    ClaudeSessionFile,
    /// The Codex rollout this exact process has open.
    CodexRollout,
}

/// Why a swapped agent is due a follow-up. When more than one applies, the
/// first listed wins: a usage cap, then a turn the swap cut off, then a
/// policy that sends it to everyone.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, strum::Display)]
#[serde(rename_all = "snake_case")]
#[strum(serialize_all = "snake_case")]
pub enum ContinueReason {
    RateLimited,
    CutOff,
    Always,
}

/// One row of `aas candidates --json`.
#[derive(Debug, Clone, PartialEq, serde::Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Candidate {
    pub name: String,
    pub provider: String,
    #[serde(default)]
    pub eligible: bool,
    #[serde(default)]
    pub active: bool,
    #[serde(default)]
    pub reason: String,
    #[serde(default)]
    pub remaining_pct: Option<f64>,
}

/// A pane the daemon planned but left to the caller because it runs its
/// agent directly: what it was when planned, so the caller acts on that
/// agent and no other.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DirectTarget {
    pub pane: String,
    pub socket: Option<String>,
    /// `pane@server`, normalized ([`endpoint_key`]).
    pub endpoint: String,
    pub conversation_id: String,
}

/// A swap request exactly as the caller made it: only what it set, nothing
/// filled in from configuration. Its digest identifies a repeated request,
/// so changing a default between two sends of the same request does not
/// make it a different one.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
#[allow(clippy::struct_excessive_bools)] // independent request switches, as on the CLI
pub struct SwapOverrides {
    pub targets: Vec<String>,
    pub all: bool,
    pub limited: bool,
    pub provider: Option<String>,
    pub to: Option<String>,
    pub spread: bool,
    pub force: bool,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub when_idle: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub idle_timeout: Option<String>,
    pub bare: Option<String>,
    pub continue_text: Option<String>,
    pub continue_when: Option<String>,
    pub no_continue: bool,
    /// `binding` or `verified`. Left out of the digest when not typed, so a
    /// request from before the option is the same request.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub continue_evidence: Option<String>,
    pub exclude: Vec<String>,
    /// The configuration file the caller loaded, if it named one. A request
    /// planned against a different file than the daemon's is refused rather
    /// than silently run on the daemon's.
    pub caller_config: Option<PathBuf>,
}

impl SwapOverrides {
    /// Refuse the combinations the CLI refuses, whoever the caller is: none
    /// of them is given a silent winner.
    pub fn validate(&self) -> Result<(), String> {
        if self.idle_timeout.is_some() && !self.when_idle {
            return Err("idle_timeout requires when_idle".into());
        }
        if let Some(timeout) = &self.idle_timeout {
            if timeout.len() > MAX_NAME {
                return Err("idle_timeout is too long".into());
            }
            crate::automation::parse_duration(timeout)?;
        }
        let conflict = |a: &str, b: &str| Err(format!("{a} and {b} cannot be combined"));
        if !self.targets.is_empty() && self.all {
            return conflict("targets", "all");
        }
        if !self.targets.is_empty() && self.limited {
            return conflict("targets", "limited");
        }
        if self.all && self.limited {
            return conflict("all", "limited");
        }
        if self.spread && self.to.is_some() {
            return conflict("spread", "to");
        }
        if self.no_continue && self.continue_when.is_some() {
            return conflict("no_continue", "continue_when");
        }
        if self.no_continue && self.continue_text.is_some() {
            return conflict("no_continue", "continue_text");
        }
        if let Some(when) = self.continue_when.as_deref() {
            if !matches!(when, "limited" | "always" | "never") {
                return Err(format!("unknown continue policy {when:?}"));
            }
        }
        if let Some(evidence) = self.continue_evidence.as_deref() {
            if !matches!(evidence, "binding" | "verified") {
                return Err(format!("unknown continue evidence {evidence:?}"));
            }
        }
        let names = |what: &str, names: &[String]| {
            if names.len() > MAX_NAMES || names.iter().any(|name| name.len() > MAX_NAME) {
                Err(format!(
                    "{what}: at most {MAX_NAMES} names of up to {MAX_NAME} bytes"
                ))
            } else {
                Ok(())
            }
        };
        names("targets", &self.targets)?;
        names("exclude", &self.exclude)?;
        for (what, value) in [
            ("provider", self.provider.as_deref()),
            ("to", self.to.as_deref()),
            ("bare", self.bare.as_deref()),
        ] {
            if value.is_some_and(|value| value.len() > MAX_NAME) {
                return Err(format!("{what}: longer than {MAX_NAME} bytes"));
            }
        }
        if self
            .continue_text
            .as_deref()
            .is_some_and(|text| text.len() > MAX_TEXT)
        {
            return Err(format!("continue_text: longer than {MAX_TEXT} bytes"));
        }
        if self
            .caller_config
            .as_deref()
            .is_some_and(|path| path.as_os_str().len() > MAX_PATH)
        {
            return Err(format!("caller_config: longer than {MAX_PATH} bytes"));
        }
        Ok(())
    }
}

const MAX_NAMES: usize = 256;

const MAX_NAME: usize = 256;

const MAX_TEXT: usize = 4096;

const MAX_PATH: usize = 4096;

const MAX_KEY: usize = 128;

/// A request key: what a lost acknowledgement is recovered by, so it is
/// bounded and plain.
pub fn validate_key(key: &str) -> Result<(), String> {
    let plain = key
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || "-_.:@".contains(c));
    if key.is_empty() || key.len() > MAX_KEY || !plain {
        return Err(format!(
            "a request key is 1-{MAX_KEY} characters of letters, digits and -_.:@"
        ));
    }
    Ok(())
}

/// A job by id, or by the request key that made it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum JobRef {
    Id(String),
    Key(String),
}

/// One report row of a job, in the order the request reported it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EntryView {
    pub report_index: usize,
    pub pane: Option<String>,
    /// `None` while the target is still to run, or was never reached.
    pub disposition: Option<String>,
    pub reason: Option<String>,
    /// Effects whose result was never recorded: they may or may not have
    /// happened.
    pub uncertain_effects: u32,
    /// The executor's own report, while the daemon that ran it holds it.
    pub outcome: Option<SwapOutcome>,
    /// The follow-up decision the journal recorded: `allowed`, `not_due`,
    /// or why it was withheld.
    #[serde(default)]
    pub continuation: Option<String>,
    /// The follow-up the journal recorded: the transport's word, and the
    /// evidence once its wait ended.
    #[serde(default)]
    pub follow_up: Option<FollowUpReport>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct JobView {
    pub job_id: String,
    pub node_id: String,
    pub state: String,
    pub revision: u64,
    pub reason: Option<String>,
    pub entries: Vec<EntryView>,
    /// Why this job cannot go on: the service faulted while it was still
    /// accepted or running. An ended job never carries one, whatever
    /// happened to the service after it ended.
    pub fault: Option<String>,
    /// Why the service stopped taking work, if it did: its health, not this
    /// job's result.
    #[serde(default)]
    pub service_fault: Option<String>,
    /// A cancel was asked for: no target starts after it, and targets that
    /// never started read `cancelled`.
    #[serde(default)]
    pub cancel_requested: bool,
    /// When this job's effects and observations were pruned, if they were:
    /// details they held (follow-up, continuation) are expired, not absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub evidence_pruned_at: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Accepted {
    pub job_id: String,
    /// The request key was seen before: this is the job it made then.
    pub existing: bool,
    pub view: JobView,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlanView {
    pub outcomes: Vec<SwapOutcome>,
    /// Planned targets left out because they run their agent directly: a
    /// caller may act on exactly these itself.
    #[serde(default)]
    pub direct_targets: Vec<DirectTarget>,
    /// Whether a submit would be taken now.
    pub executable: bool,
    pub unavailable: Option<String>,
}

/// What a retry may set: everything else is the parent's.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct RetryOverrides {
    /// `binding` or `verified`; only matters to the relaunch's report, as a
    /// retry types no follow-up.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub continue_evidence: Option<String>,
    /// Also stop an agent that is mid-turn now.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub force: bool,
    /// The configuration file the caller named, if it named one: refused
    /// unless it is the daemon's.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub caller_config: Option<PathBuf>,
}

impl RetryOverrides {
    pub fn validate(&self) -> Result<(), String> {
        if self
            .caller_config
            .as_deref()
            .is_some_and(|path| path.as_os_str().len() > MAX_PATH)
        {
            return Err(format!("caller_config: longer than {MAX_PATH} bytes"));
        }
        self.evidence().map(|_| ())
    }

    pub fn evidence(&self) -> Result<Option<ContinueEvidence>, String> {
        match self.continue_evidence.as_deref() {
            None => Ok(None),
            Some("binding") => Ok(Some(ContinueEvidence::Binding)),
            Some("verified") => Ok(Some(ContinueEvidence::Verified)),
            Some(other) => Err(format!("unknown continue evidence {other:?}")),
        }
    }
}

/// The automation's check that its swap is still wanted: `Err` says why not.
pub type StillDue = Box<dyn Fn() -> Result<(), String> + Send>;

/// Whether the swap service takes jobs now, and if not why: read-only.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SwapHealth {
    pub readiness: SwapReadiness,
    /// Why it is not `ready`.
    pub reason: Option<String>,
    /// The job store's schema, once one was opened; `None` if it never was.
    pub schema: Option<i64>,
    /// The schema this build writes and reads.
    pub expected_schema: i64,
    /// The fault that halted it, if one did (apart from other reasons).
    pub fault: Option<String>,
    /// Jobs this manager is running now (not rows an earlier manager left).
    pub jobs_running: usize,
    /// Why the last pruning of old jobs' evidence failed, if it did: upkeep,
    /// not a fault; jobs are taken regardless.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub maintenance: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, strum::Display)]
#[serde(rename_all = "snake_case")]
#[strum(serialize_all = "snake_case")]
pub enum SwapReadiness {
    Ready,
    /// Shutting down: no new job.
    Draining,
    /// A fault stopped it; it takes nothing until the daemon restarts.
    Halted,
    /// It never opened, or its store is no longer its own.
    Unavailable,
}

/// The swap service a daemon could not open, kept for its reason: every
/// request is refused with it, and health reports it.
pub struct UnopenedSwap {
    reason: String,
    expected_schema: i64,
    module_off: bool,
}

impl UnopenedSwap {
    #[must_use]
    pub fn new(reason: String, expected_schema: i64) -> Self {
        Self {
            reason,
            expected_schema,
            module_off: false,
        }
    }

    /// The account module is off for this daemon (not enabled by its
    /// config, or not in this build), as opposed to a service that failed
    /// to open.
    #[must_use]
    pub fn module_off(reason: String, expected_schema: i64) -> Self {
        Self {
            module_off: true,
            ..Self::new(reason, expected_schema)
        }
    }

    fn refused<T>(&self) -> Result<T, String> {
        Err(format!(
            "swap jobs are unavailable on this daemon: {}",
            self.reason
        ))
    }
}

impl SwapJobs for UnopenedSwap {
    fn active(&self) -> Result<(), String> {
        self.refused()
    }

    fn plan(&self, _: &SwapOverrides) -> Result<PlanView, String> {
        self.refused()
    }

    fn submit(&self, _: &str, _: &SwapOverrides) -> Result<Accepted, String> {
        self.refused()
    }

    fn retry(&self, _: &str, _: &JobRef, _: &RetryOverrides) -> Result<Accepted, String> {
        self.refused()
    }

    fn cancel(&self, _: &JobRef) -> Result<JobView, String> {
        self.refused()
    }

    fn status(&self, _: &JobRef) -> Result<JobView, String> {
        self.refused()
    }

    fn health(&self) -> SwapHealth {
        SwapHealth {
            readiness: SwapReadiness::Unavailable,
            reason: Some(self.reason.clone()),
            schema: None,
            expected_schema: self.expected_schema,
            fault: None,
            jobs_running: 0,
            maintenance: None,
        }
    }

    fn exists(&self) -> bool {
        false
    }

    fn module_off(&self) -> bool {
        self.module_off
    }
}

/// What an IPC caller may do with swap jobs. Every request through it is a
/// public one: it can never claim the daemon's own automation privileges.
pub trait SwapJobs: Send + Sync {
    /// Read account metadata without planning or acting on an agent.
    fn accounts(&self) -> Result<Vec<ProviderCandidates>, String> {
        Err("account candidates are unavailable".into())
    }

    fn active(&self) -> Result<(), String>;
    fn plan(&self, overrides: &SwapOverrides) -> Result<PlanView, String>;
    fn submit(&self, request_key: &str, overrides: &SwapOverrides) -> Result<Accepted, String>;
    fn retry(
        &self,
        request_key: &str,
        parent: &JobRef,
        overrides: &RetryOverrides,
    ) -> Result<Accepted, String>;
    /// Ask a job to start no further target; accepted whatever the
    /// service's health.
    fn cancel(&self, job: &JobRef) -> Result<JobView, String>;
    /// How the service is, read-only: never plans, never touches a pane.
    fn health(&self) -> SwapHealth;
    /// A service that opened (it can have jobs), as opposed to the reason
    /// one did not.
    fn exists(&self) -> bool {
        true
    }
    /// The account module is off: nothing account-related will answer
    /// until the config turns it on and tendd restarts. `swap_health`
    /// reports it as `module_off` (`swap_health_module_v1`).
    fn module_off(&self) -> bool {
        false
    }
    /// Readable whatever state the service is in, so a halted or recovered
    /// job can always be inspected.
    fn status(&self, job: &JobRef) -> Result<JobView, String>;
}

/// Public account metadata only; no environment values or credential paths.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct ProviderCandidates {
    pub provider: String,
    pub source: String,
    pub reason: Option<String>,
    pub candidates: Vec<Candidate>,
}

// ---------------------------------------------------------------------------
// Launch routing: `[agent.<program>].account`, as config reads it
// ---------------------------------------------------------------------------

/// The providers accounts are routed for.
pub const PROVIDERS: &[&str] = &["claude", "codex"];

/// Which account a new agent starts on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SpawnAccount {
    /// The first configured account, or `aas`'s best-ranked one with quota
    /// left.
    Auto,
    /// This account, by name (`work`, or `work@claude` as `aas` spells it).
    Named(String),
}

impl SpawnAccount {
    /// Read `[agent.<program>].account`. `None` for a blank value.
    #[must_use]
    pub fn parse(raw: &str) -> Option<Self> {
        match raw.trim() {
            "" => None,
            "auto" => Some(Self::Auto),
            name => Some(Self::Named(name.to_owned())),
        }
    }
}

/// A provider's routing policy, resolved from config for one launch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AccountRoute {
    /// `claude` or `codex`.
    pub provider: String,
    pub choice: SpawnAccount,
    /// The configured `aas` binary (`[swap].aas`), used when the provider
    /// lists no [`Self::profiles`].
    pub aas: String,
    /// The provider's `[[accounts.<provider>]]`, in config order. When
    /// present, routing needs no external tool.
    pub profiles: Vec<crate::accounts::AccountProfile>,
    /// `[modules] accounts = false` with an account asked for: the launch
    /// starts bare and says why.
    pub module_off: bool,
}

/// What routing decided, reported with the launch result.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AccountDecision {
    /// The account the agent was started on; `None` when it started bare.
    pub account: Option<String>,
    /// Why it started bare, when it did.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub skipped: Option<String>,
}

/// A decided launch: the words that start the provider CLI on `account`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RoutedLaunch {
    pub account: String,
    /// `aas e <account>` (with `aas` as an absolute path where one was
    /// found: the pane's shell need not have this process's `PATH`), or a
    /// configured account's `env K=V … <provider>`. Either way it ends where
    /// the provider's own arguments begin.
    pub launcher: Vec<String>,
}

impl RoutedLaunch {
    /// The argv that starts `provider_args` on this account.
    #[must_use]
    pub fn argv(&self, provider_args: impl IntoIterator<Item = String>) -> Vec<String> {
        self.launcher.iter().cloned().chain(provider_args).collect()
    }

    /// Through `aas`, which picks the provider CLI from the account.
    #[must_use]
    pub fn via_aas(aas: String, account: String) -> Self {
        Self {
            launcher: vec![aas, "e".into(), account.clone()],
            account,
        }
    }
}

/// `decide`, folded into what the launch result reports.
#[must_use]
pub fn report(outcome: &Result<RoutedLaunch, String>) -> AccountDecision {
    match outcome {
        Ok(routed) => AccountDecision {
            account: Some(routed.account.clone()),
            skipped: None,
        },
        Err(reason) => AccountDecision {
            account: None,
            skipped: Some(reason.clone()),
        },
    }
}
