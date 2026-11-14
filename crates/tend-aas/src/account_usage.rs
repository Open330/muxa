//! What tend has seen of each configured account's quota, so a launch or a
//! swap can rank `[[accounts.<provider>]]` by observed usage instead of by
//! config order alone.
//!
//! **Attribution is by the process, never by timing.** A reading belongs to
//! an account only when it was taken while the agent's own process ran on
//! that account: the process's environment matches exactly one configured
//! profile ([`tend::accounts::ResolvedAccounts::identify`]), the same check a
//! swap uses to tell which account an agent is on. A process whose
//! environment cannot be read, or matches no profile, credits nothing.
//!
//! **A reading counts only once it changes under that process.** An agent
//! row outlives its process: after a swap the relaunched agent resumes the
//! same row, which still shows the old account's percentages until the new
//! process reports its own. So the first sight of a process (`pid` and its
//! start time) is only a baseline, and each window is credited only when
//! its own value changes while the same process runs. A partial heartbeat
//! that updates one window cannot carry the other window's old value over.
//! A cap is credited when the row turns rate-limited under the same process.
//!
//! The ledger lives at `<data dir>/tend/account-usage.json`. tendd is its only
//! writer; launches and swaps only read it. Missing, unreadable or newer
//! versions read as empty, which ranks in config order exactly as before.

use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use time::{Duration, OffsetDateTime};

/// The ledger's format version.
pub const VERSION: u32 = 1;

/// How long an account is held as capped when its cap carried no reset time.
pub const UNKNOWN_RESET_COOLDOWN: Duration = Duration::hours(1);

/// A quota window an agent reports.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Window {
    FiveHour,
    SevenDay,
}

impl Window {
    const ALL: [Self; 2] = [Self::FiveHour, Self::SevenDay];

    fn length(self) -> Duration {
        match self {
            Self::FiveHour => Duration::hours(5),
            Self::SevenDay => Duration::days(7),
        }
    }
}

/// One window's last credited reading.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct WindowReading {
    pub used_pct: f32,
    #[serde(default, with = "time::serde::rfc3339::option")]
    pub resets_at: Option<OffsetDateTime>,
    #[serde(with = "time::serde::rfc3339")]
    pub observed_at: OffsetDateTime,
}

impl WindowReading {
    /// Whether the reading still describes the window: its reset has not
    /// passed, or, with no reset time, the window's length has not.
    fn current(&self, window: Window, now: OffsetDateTime) -> bool {
        match self.resets_at {
            Some(reset) => reset > now,
            None => self.observed_at + window.length() > now,
        }
    }
}

/// What the ledger holds for one account.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct AccountUsage {
    #[serde(default)]
    pub windows: BTreeMap<Window, WindowReading>,
    /// Until when an agent on this account was seen capped.
    #[serde(default, with = "time::serde::rfc3339::option")]
    pub capped_until: Option<OffsetDateTime>,
}

/// An account's standing now, as the ranking reads it.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct Standing {
    /// Capped until then: not a candidate.
    pub capped_until: Option<OffsetDateTime>,
    /// The fullest current window's use, when any window is current.
    pub used_pct: Option<f32>,
}

/// Every account's observed usage, keyed `<provider>/<account>`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Ledger {
    pub version: u32,
    #[serde(default)]
    pub accounts: BTreeMap<String, AccountUsage>,
}

impl Default for Ledger {
    fn default() -> Self {
        Self {
            version: VERSION,
            accounts: BTreeMap::new(),
        }
    }
}

fn key(provider: &str, account: &str) -> String {
    format!("{provider}/{account}")
}

impl Ledger {
    /// `account`'s standing at `now`. A window at 100% counts as a cap
    /// until it resets.
    #[must_use]
    pub fn standing(&self, provider: &str, account: &str, now: OffsetDateTime) -> Standing {
        let Some(usage) = self.accounts.get(&key(provider, account)) else {
            return Standing::default();
        };
        let current: Vec<(Window, &WindowReading)> = usage
            .windows
            .iter()
            .filter(|(window, reading)| reading.current(**window, now))
            .map(|(window, reading)| (*window, reading))
            .collect();
        let full_until = current
            .iter()
            .filter(|(_, reading)| reading.used_pct >= 100.0)
            .map(|(window, reading)| {
                reading
                    .resets_at
                    .unwrap_or(reading.observed_at + window.length())
            })
            .max();
        let capped_until = usage
            .capped_until
            .filter(|until| *until > now)
            .into_iter()
            .chain(full_until)
            .max();
        let used_pct = current
            .iter()
            .map(|(_, reading)| reading.used_pct)
            .reduce(f32::max);
        Standing {
            capped_until,
            used_pct,
        }
    }

    /// Drop what no longer says anything at `now`, so the file stays small.
    pub fn prune(&mut self, now: OffsetDateTime) {
        for usage in self.accounts.values_mut() {
            usage
                .windows
                .retain(|window, reading| reading.current(*window, now));
            usage.capped_until = usage.capped_until.filter(|until| *until > now);
        }
        self.accounts
            .retain(|_, usage| !usage.windows.is_empty() || usage.capped_until.is_some());
    }
}

/// `<data dir>/tend/account-usage.json`.
#[must_use]
pub fn default_path() -> Option<PathBuf> {
    Some(
        tend::paths::data_dir()?
            .join("tend")
            .join("account-usage.json"),
    )
}

/// The ledger at `path`; empty when it is missing, unreadable, or of
/// another version.
#[must_use]
pub fn load(path: &Path) -> Ledger {
    std::fs::read(path)
        .ok()
        .and_then(|bytes| serde_json::from_slice::<Ledger>(&bytes).ok())
        .filter(|ledger| ledger.version == VERSION)
        .unwrap_or_default()
}

/// [`load`] from [`default_path`].
#[must_use]
pub fn load_default() -> Ledger {
    default_path().map(|path| load(&path)).unwrap_or_default()
}

/// Write `ledger` to `path` whole: a reader sees the old file or the new.
pub fn save(path: &Path, ledger: &Ledger) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let tmp = path.with_extension(format!("json.tmp.{}", std::process::id()));
    std::fs::write(&tmp, serde_json::to_vec_pretty(ledger)?)?;
    std::fs::rename(&tmp, path).inspect_err(|_| {
        let _ = std::fs::remove_file(&tmp);
    })
}

/// One agent row as the recorder sees it on one pass.
#[derive(Debug, Clone, PartialEq)]
pub struct Reading {
    /// The agent row (its pane, server and session).
    pub row: String,
    /// The process running in it now: pid and start time.
    pub process: (u32, String),
    pub provider: String,
    pub five_hour: Option<(f32, Option<OffsetDateTime>)>,
    pub seven_day: Option<(f32, Option<OffsetDateTime>)>,
    /// Whether the row shows the agent capped.
    pub capped: bool,
    /// Until when, if the cap said.
    pub capped_until: Option<OffsetDateTime>,
}

impl Reading {
    fn window(&self, window: Window) -> Option<(f32, Option<OffsetDateTime>)> {
        match window {
            Window::FiveHour => self.five_hour,
            Window::SevenDay => self.seven_day,
        }
    }
}

#[derive(Debug, Clone)]
struct Seen {
    process: (u32, String),
    last: Reading,
    /// Whether `account` was looked up yet.
    asked: bool,
    /// The account this process runs on: `None` for none, or unreadable.
    account: Option<String>,
}

/// Turns successive passes over the agent rows into ledger credits.
#[derive(Debug, Default)]
pub struct Recorder {
    seen: HashMap<String, Seen>,
}

impl Recorder {
    /// Credit what changed since the last pass to the account each change
    /// happened on. `account_of` names the account a process runs on; it is
    /// asked at most once per process, and only when there is something to
    /// credit. Returns whether `ledger` changed.
    pub fn observe(
        &mut self,
        readings: Vec<Reading>,
        mut account_of: impl FnMut(&Reading) -> Option<String>,
        ledger: &mut Ledger,
        now: OffsetDateTime,
    ) -> bool {
        let mut changed = false;
        let rows: std::collections::HashSet<String> =
            readings.iter().map(|r| r.row.clone()).collect();
        self.seen.retain(|row, _| rows.contains(row));
        for reading in readings {
            let seen = match self.seen.get_mut(&reading.row) {
                Some(seen) if seen.process == reading.process => seen,
                _ => {
                    // A new process: whatever the row shows was not
                    // necessarily reported by it.
                    self.seen.insert(
                        reading.row.clone(),
                        Seen {
                            process: reading.process.clone(),
                            last: reading,
                            asked: false,
                            account: None,
                        },
                    );
                    continue;
                }
            };
            let windows: Vec<(Window, f32, Option<OffsetDateTime>)> = Window::ALL
                .into_iter()
                .filter_map(|window| {
                    let (pct, reset) = reading.window(window)?;
                    (seen.last.window(window) != Some((pct, reset))).then_some((window, pct, reset))
                })
                .collect();
            let capped = (!seen.last.capped && reading.capped)
                .then(|| reading.capped_until.unwrap_or(now + UNKNOWN_RESET_COOLDOWN));
            if !windows.is_empty() || capped.is_some() {
                if !seen.asked {
                    seen.account = account_of(&reading);
                    seen.asked = true;
                }
                if let Some(account) = seen.account.clone() {
                    let usage = ledger
                        .accounts
                        .entry(key(&reading.provider, &account))
                        .or_default();
                    for (window, used_pct, resets_at) in windows {
                        usage.windows.insert(
                            window,
                            WindowReading {
                                used_pct,
                                resets_at,
                                observed_at: now,
                            },
                        );
                    }
                    if let Some(until) = capped {
                        usage.capped_until = usage.capped_until.max(Some(until));
                    }
                    changed = true;
                }
            }
            seen.last = reading;
        }
        changed
    }
}

/// The reading of `agent`, whose process is `process`: `None` for a
/// provider without configured accounts.
#[must_use]
pub fn reading_of(agent: &tend::state::Agent, process: (u32, String)) -> Option<Reading> {
    let provider = crate::account_swap::provider_name(agent.kind)?;
    Some(Reading {
        row: format!(
            "{}@{}/{}",
            agent.pane.as_deref().unwrap_or_default(),
            agent.tmux_socket.as_deref().unwrap_or_default(),
            agent.session_id
        ),
        process,
        provider: provider.to_owned(),
        five_hour: agent
            .rate_limit_5h_pct
            .map(|pct| (pct, agent.rate_limit_5h_resets_at)),
        seven_day: agent
            .rate_limit_7d_pct
            .map(|pct| (pct, agent.rate_limit_7d_resets_at)),
        capped: crate::account_swap::is_rate_limited(agent),
        capped_until: agent.rate_limited_until,
    })
}

/// One recorder pass over the daemon's agents: each agent of a provider
/// that lists `[[accounts]]` is found in its pane (the same lookup a swap
/// plans with), and credited through `recorder`. Does nothing at all when
/// no provider lists accounts. Returns whether `ledger` changed.
pub fn record_pass(
    recorder: &mut Recorder,
    ledger: &mut Ledger,
    agents: &[tend::state::Agent],
    backends: &[tend::SharedBackend],
    accounts: &tend::accounts::AccountsConfig,
    now: OffsetDateTime,
) -> bool {
    let resolved: BTreeMap<&str, tend::accounts::ResolvedAccounts> = tend::swap_api::PROVIDERS
        .iter()
        .filter(|provider| !accounts.for_provider(provider).is_empty())
        .map(|provider| {
            (
                *provider,
                tend::accounts::ResolvedAccounts::new(accounts.for_provider(provider)),
            )
        })
        .collect();
    if resolved.is_empty() {
        return false;
    }
    let mut panes: HashMap<tend::backend::HostKind, Vec<tend::tmux::PaneInfo>> = HashMap::new();
    let mut readings = Vec::new();
    for agent in agents {
        let Some(provider) = crate::account_swap::provider_name(agent.kind) else {
            continue;
        };
        if !resolved.contains_key(provider) || agent.state == tend::event::AgentState::Stopped {
            continue;
        }
        let Some(pane_id) = agent.pane.as_deref() else {
            continue;
        };
        let Some(host) = tend::backend::pane_id_host_kind(pane_id) else {
            continue;
        };
        let listed = panes.entry(host).or_insert_with(|| {
            backends
                .iter()
                .filter(|backend| backend.kind() == host)
                .flat_map(|backend| backend.list_panes())
                .collect()
        });
        let Some(pane) = listed.iter().find(|pane| {
            pane.pane_id == pane_id
                && agent
                    .tmux_socket
                    .as_deref()
                    .is_none_or(|socket| pane.socket.as_deref().is_none_or(|s| s == socket))
        }) else {
            continue;
        };
        if pane.pane_pid == 0 {
            continue;
        }
        let processes = tend::pane_process::pane_processes(pane.pane_pid);
        let Some(launch) = crate::account_swap::find_launch(&processes, pane.pane_pid, agent.kind)
        else {
            continue;
        };
        let Some(start) = tend::agent_conversation::process_start(launch.agent_pid) else {
            continue;
        };
        if let Some(reading) = reading_of(agent, (launch.agent_pid, start)) {
            readings.push(reading);
        }
    }
    recorder.observe(
        readings,
        |reading| {
            let (pid, start) = &reading.process;
            let environ = tend::accounts::process_environ(*pid);
            // The environment must be the one of the process just seen.
            if environ.is_empty()
                || tend::agent_conversation::process_start(*pid).as_ref() != Some(start)
            {
                return None;
            }
            resolved
                .get(reading.provider.as_str())?
                .identify(&environ)
                .map(str::to_owned)
        },
        ledger,
        now,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(minutes: i64) -> OffsetDateTime {
        OffsetDateTime::from_unix_timestamp(1_800_000_000).unwrap() + Duration::minutes(minutes)
    }

    fn reading(pid: u32, five: Option<f32>, seven: Option<f32>) -> Reading {
        Reading {
            row: "%1@default/s".into(),
            process: (pid, format!("start-{pid}")),
            provider: "claude".into(),
            five_hour: five.map(|pct| (pct, Some(at(120)))),
            seven_day: seven.map(|pct| (pct, Some(at(6000)))),
            capped: false,
            capped_until: None,
        }
    }

    fn accounts<'a>(by_pid: &'a [(u32, &'a str)]) -> impl FnMut(&Reading) -> Option<String> + 'a {
        |reading: &Reading| {
            by_pid
                .iter()
                .find(|(pid, _)| *pid == reading.process.0)
                .map(|(_, name)| (*name).to_owned())
        }
    }

    #[test]
    fn first_sight_of_a_process_credits_nothing() {
        let mut recorder = Recorder::default();
        let mut ledger = Ledger::default();
        let changed = recorder.observe(
            vec![reading(10, Some(40.0), Some(70.0))],
            accounts(&[(10, "a")]),
            &mut ledger,
            at(0),
        );
        assert!(!changed);
        assert!(ledger.accounts.is_empty());
    }

    #[test]
    fn a_change_under_the_same_process_is_credited_to_its_account() {
        let mut recorder = Recorder::default();
        let mut ledger = Ledger::default();
        let by_pid = [(10, "a")];
        recorder.observe(
            vec![reading(10, Some(40.0), Some(70.0))],
            accounts(&by_pid),
            &mut ledger,
            at(0),
        );
        assert!(recorder.observe(
            vec![reading(10, Some(45.0), Some(70.0))],
            accounts(&by_pid),
            &mut ledger,
            at(1),
        ));
        let usage = &ledger.accounts["claude/a"];
        assert_eq!(usage.windows[&Window::FiveHour].used_pct, 45.0);
        // The 7-day window did not change under this process: not credited.
        assert!(!usage.windows.contains_key(&Window::SevenDay));
        assert_eq!(ledger.standing("claude", "a", at(2)).used_pct, Some(45.0));
    }

    #[test]
    fn a_relaunch_does_not_carry_the_old_accounts_reading_to_the_new_one() {
        // Account a hits 100% on 7d; the swap relaunches on b in the same
        // row, which still shows 100% until b's process reports.
        let mut recorder = Recorder::default();
        let mut ledger = Ledger::default();
        let by_pid = [(10, "a"), (20, "b")];
        recorder.observe(
            vec![reading(10, Some(50.0), Some(90.0))],
            accounts(&by_pid),
            &mut ledger,
            at(0),
        );
        recorder.observe(
            vec![reading(10, Some(50.0), Some(100.0))],
            accounts(&by_pid),
            &mut ledger,
            at(1),
        );
        recorder.observe(
            vec![reading(20, Some(50.0), Some(100.0))],
            accounts(&by_pid),
            &mut ledger,
            at(2),
        );
        // A partial heartbeat from b: only its 5h moved.
        recorder.observe(
            vec![reading(20, Some(3.0), Some(100.0))],
            accounts(&by_pid),
            &mut ledger,
            at(3),
        );
        let a = ledger.standing("claude", "a", at(4));
        assert_eq!(a.capped_until, Some(at(6000)));
        let b = ledger.standing("claude", "b", at(4));
        assert_eq!(b.capped_until, None);
        assert_eq!(b.used_pct, Some(3.0));
    }

    #[test]
    fn a_cap_is_credited_on_the_transition_and_lapses() {
        let mut recorder = Recorder::default();
        let mut ledger = Ledger::default();
        let by_pid = [(10, "a")];
        recorder.observe(
            vec![reading(10, None, None)],
            accounts(&by_pid),
            &mut ledger,
            at(0),
        );
        let mut capped = reading(10, None, None);
        capped.capped = true;
        capped.capped_until = Some(at(30));
        assert!(recorder.observe(vec![capped.clone()], accounts(&by_pid), &mut ledger, at(1)));
        assert_eq!(
            ledger.standing("claude", "a", at(2)).capped_until,
            Some(at(30))
        );
        assert_eq!(ledger.standing("claude", "a", at(31)).capped_until, None);
        // Still capped on the next pass: no new credit.
        assert!(!recorder.observe(vec![capped], accounts(&by_pid), &mut ledger, at(2)));
        // A cap with no reset time holds for the cooldown.
        let mut recorder = Recorder::default();
        let mut ledger = Ledger::default();
        recorder.observe(
            vec![reading(10, None, None)],
            accounts(&by_pid),
            &mut ledger,
            at(0),
        );
        let mut unknown = reading(10, None, None);
        unknown.capped = true;
        recorder.observe(vec![unknown], accounts(&by_pid), &mut ledger, at(1));
        assert_eq!(
            ledger.standing("claude", "a", at(1)).capped_until,
            Some(at(1) + UNKNOWN_RESET_COOLDOWN)
        );
    }

    #[test]
    fn an_unidentified_process_credits_nothing_and_is_asked_once() {
        let mut recorder = Recorder::default();
        let mut ledger = Ledger::default();
        let mut asked = 0;
        let mut none = |_: &Reading| {
            asked += 1;
            None
        };
        recorder.observe(
            vec![reading(10, Some(1.0), None)],
            &mut none,
            &mut ledger,
            at(0),
        );
        recorder.observe(
            vec![reading(10, Some(2.0), None)],
            &mut none,
            &mut ledger,
            at(1),
        );
        recorder.observe(
            vec![reading(10, Some(3.0), None)],
            &mut none,
            &mut ledger,
            at(2),
        );
        assert_eq!(asked, 1);
        assert!(ledger.accounts.is_empty());
    }

    #[test]
    fn readings_lapse_at_their_reset_or_after_their_window() {
        let mut ledger = Ledger::default();
        let usage = ledger.accounts.entry(key("codex", "a")).or_default();
        usage.windows.insert(
            Window::FiveHour,
            WindowReading {
                used_pct: 100.0,
                resets_at: Some(at(10)),
                observed_at: at(0),
            },
        );
        usage.windows.insert(
            Window::SevenDay,
            WindowReading {
                used_pct: 20.0,
                resets_at: None,
                observed_at: at(0),
            },
        );
        let now = ledger.standing("codex", "a", at(5));
        assert_eq!(now.capped_until, Some(at(10)));
        assert_eq!(now.used_pct, Some(100.0));
        let later = ledger.standing("codex", "a", at(11));
        assert_eq!(later.capped_until, None);
        assert_eq!(later.used_pct, Some(20.0));
        ledger.prune(at(8 * 24 * 60));
        assert!(ledger.accounts.is_empty());
    }

    #[test]
    fn the_ledger_round_trips_and_other_versions_read_as_empty() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("tend/account-usage.json");
        assert_eq!(load(&path), Ledger::default());
        let mut ledger = Ledger::default();
        ledger
            .accounts
            .entry(key("claude", "a"))
            .or_default()
            .capped_until = Some(at(5));
        save(&path, &ledger).unwrap();
        assert_eq!(load(&path), ledger);
        std::fs::write(&path, r#"{"version":2,"accounts":{}}"#).unwrap();
        assert_eq!(load(&path), Ledger::default());
        std::fs::write(&path, "not json").unwrap();
        assert_eq!(load(&path), Ledger::default());
    }
}
