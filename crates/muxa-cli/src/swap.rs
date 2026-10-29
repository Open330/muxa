//! `muxa swap` — move agents onto another `aas` account and resume them.
//!
//! The work is [`muxa::account_swap::run`], shared with the `swap_account`
//! automation action; this module turns flags into a request and prints the
//! per-agent report.

use anyhow::{bail, Context, Result};
use clap::{Args as ClapArgs, ValueEnum};
use muxa::account_swap::{
    self, AccountChoice, BareMode, ContinueWhen, SwapConfig, SwapOutcome, SwapRequest, SwapScope,
    SwapStatus,
};
use muxa::event::AgentKind;
use muxa::ipc::Client;
use std::fmt::Write as _;

/// Move agents whose account ran out onto another `aas` account, resuming
/// each conversation in place.
///
/// With no target, swaps every agent stopped by a usage cap. Each agent is
/// stopped, its launch line is typed back into the pane's shell as
/// `aas e <next> … --resume <id>`, and a follow-up prompt carries the
/// cut-off turn on. Defaults come from `[swap]` in config.toml.
#[derive(Debug, ClapArgs)]
#[command(after_help = "Examples:
  muxa swap                     swap every rate-limited agent (auto-picked account)
  muxa swap %5 --to jelly@claude
  muxa swap --all --spread      move every idle agent, spread across accounts
  muxa swap --all --force --provider codex
  muxa swap --dry-run           show the plan, touch nothing
  muxa swap --host rtzr --to work@claude   swap on a Fleet host")]
#[allow(clippy::struct_excessive_bools)] // independent CLI switches
pub struct Args {
    /// Panes (`%5`, `herdr:p9`) or provider session ids to swap.
    #[arg(value_name = "TARGET", conflicts_with_all = ["all", "limited"])]
    targets: Vec<String>,
    /// Every Claude / Codex agent, not only the rate-limited ones.
    #[arg(long, conflicts_with = "limited")]
    all: bool,
    /// Only agents stopped by a usage cap (the default with no target).
    #[arg(long)]
    limited: bool,
    /// Only this provider's agents.
    #[arg(long, value_enum)]
    provider: Option<Provider>,
    /// Account to move to: an `aas` name, or `auto` for the best-ranked
    /// account with quota left.
    #[arg(
        long,
        value_name = "ACCOUNT",
        default_value = "auto",
        conflicts_with = "spread"
    )]
    to: String,
    /// Round-robin the agents over every account with quota left.
    #[arg(long)]
    spread: bool,
    /// Also stop agents that are mid-turn.
    #[arg(long)]
    force: bool,
    /// Agents started without `aas`: leave them (`skip`) or relaunch them
    /// under it (`wrap`). Default from `[swap] bare`.
    #[arg(long, value_enum)]
    bare: Option<Bare>,
    /// Follow-up prompt typed after the resume — the wording only; who gets
    /// it is `--continue-when`. Default from `[swap] continue_text`.
    #[arg(
        long = "continue-text",
        value_name = "TEXT",
        conflicts_with_all = ["no_continue", "continue_legacy"]
    )]
    continue_text: Option<String>,
    /// Deprecated: use `--continue-text`. Sets the wording only — it no
    /// longer sends to every swapped agent; add `--continue-when always` for
    /// that.
    #[arg(long = "continue", value_name = "TEXT", conflicts_with = "no_continue")]
    continue_legacy: Option<String>,
    /// Which swapped agents get the follow-up: `limited` (their turn was cut
    /// off by a usage cap or by this swap), `always`, or `never`. Default
    /// from `[swap] continue_when`.
    #[arg(long = "continue-when", value_enum, conflicts_with = "no_continue")]
    continue_when: Option<When>,
    /// Send no follow-up prompt (`--continue-when never`).
    #[arg(long)]
    no_continue: bool,
    /// Print the plan without stopping or typing anything.
    #[arg(long)]
    dry_run: bool,
    #[arg(long)]
    json: bool,
    /// Run the swap on this Fleet host (a control-mode `[fleet.hosts]`
    /// alias) instead of here: its daemon, its agents, its config.
    #[arg(long, value_name = "ALIAS")]
    host: Option<String>,
    /// Leave this pane alone whatever the scope (repeatable).
    #[arg(long, value_name = "PANE")]
    exclude: Vec<String>,
}

#[derive(Debug, Clone, Copy, ValueEnum)]
enum Provider {
    Claude,
    Codex,
}

#[derive(Debug, Clone, Copy, ValueEnum)]
enum Bare {
    Skip,
    Wrap,
}

#[derive(Debug, Clone, Copy, ValueEnum)]
enum When {
    Limited,
    Always,
    Never,
}

impl From<When> for ContinueWhen {
    fn from(when: When) -> Self {
        match when {
            When::Limited => Self::Limited,
            When::Always => Self::Always,
            When::Never => Self::Never,
        }
    }
}

impl Args {
    /// The follow-up wording from either spelling.
    fn follow_up_text(&self) -> Option<&String> {
        self.continue_text
            .as_ref()
            .or(self.continue_legacy.as_ref())
    }
}

/// Printed when the deprecated spelling is used: it no longer means "send
/// to every agent", and a caller relying on that should say so.
const CONTINUE_DEPRECATED: &str = "muxa swap: --continue is deprecated; use --continue-text. \
     It now keeps the configured continue_when policy (only agents whose turn was cut off); \
     add --continue-when always to send it to every swapped agent";

fn request(args: &Args, config: &SwapConfig) -> SwapRequest {
    let scope = if !args.targets.is_empty() {
        SwapScope::Targets(args.targets.clone())
    } else if args.all {
        SwapScope::All
    } else {
        SwapScope::Limited
    };
    let mut request = SwapRequest::new(scope, config);
    request.provider = args.provider.map(|provider| match provider {
        Provider::Claude => AgentKind::ClaudeCode,
        Provider::Codex => AgentKind::Codex,
    });
    request.to = if args.spread {
        AccountChoice::Spread
    } else {
        AccountChoice::parse(&args.to)
    };
    request.force = args.force;
    if let Some(bare) = args.bare {
        request.bare = match bare {
            Bare::Skip => BareMode::Skip,
            Bare::Wrap => BareMode::Wrap,
        };
    }
    // The wording and who gets it are separate: naming the prompt does not
    // widen it to agents that had nothing cut off.
    if let Some(text) = args.follow_up_text() {
        request.continue_text = Some(text.clone()).filter(|text| !text.trim().is_empty());
    }
    if args.no_continue {
        request.continue_when = ContinueWhen::Never;
    } else if let Some(when) = args.continue_when {
        request.continue_when = when.into();
    }
    request.dry_run = args.dry_run;
    request.exclude.clone_from(&args.exclude);
    request
}

pub(crate) async fn run(
    client: &Client,
    args: Args,
    config: &SwapConfig,
    accounts: &muxa::accounts::AccountsConfig,
) -> Result<()> {
    if args.continue_legacy.is_some() {
        eprintln!("{CONTINUE_DEPRECATED}");
    }
    if let Some(host) = muxa::work_control::remote_host_alias(args.host.as_deref()) {
        return run_remote(client, &args, host).await;
    }
    let request = request(&args, config);
    let agents = client.snapshot().await.context("listing agents")?;
    let backends = muxa::backend::active_backends();
    let config = config.clone();
    let accounts = accounts.clone();
    let outcomes = tokio::task::spawn_blocking(move || {
        account_swap::run(&request, &agents, &backends, &config, &accounts)
    })
    .await
    .context("swap worker")?;
    if args.json {
        println!("{}", serde_json::to_string_pretty(&outcomes)?);
    } else {
        print!("{}", render(&outcomes));
    }
    if outcomes.iter().any(|o| o.status == SwapStatus::Failed) {
        bail!("some agents could not be swapped");
    }
    Ok(())
}

/// The same swap as a `muxa swap … --json` argv for a Fleet host, every
/// value as one `--flag=value` word as the relay's allowlist expects.
fn remote_argv(args: &Args) -> Vec<String> {
    let mut words = vec!["swap".to_owned()];
    words.extend(args.targets.iter().cloned());
    for (on, flag) in [
        (args.all, "--all"),
        (args.limited, "--limited"),
        (args.spread, "--spread"),
        (args.force, "--force"),
        (args.no_continue, "--no-continue"),
        (args.dry_run, "--dry-run"),
    ] {
        if on {
            words.push(flag.to_owned());
        }
    }
    if let Some(provider) = args.provider {
        words.push(format!(
            "--provider={}",
            match provider {
                Provider::Claude => "claude",
                Provider::Codex => "codex",
            }
        ));
    }
    if !args.spread {
        words.push(format!("--to={}", args.to));
    }
    if let Some(bare) = args.bare {
        words.push(format!(
            "--bare={}",
            match bare {
                Bare::Skip => "skip",
                Bare::Wrap => "wrap",
            }
        ));
    }
    // The new spellings only: a host too old to know them refuses the run
    // rather than reading `--continue` as "send to every agent".
    // A blank wording sends nothing here, so it is `--no-continue` there:
    // the allowlist takes no empty values, and the host's own default text
    // must not stand in for one the caller cleared.
    let blank = args
        .follow_up_text()
        .is_some_and(|text| text.trim().is_empty());
    if blank && !args.no_continue {
        words.push("--no-continue".to_owned());
    } else if let Some(text) = args.follow_up_text() {
        words.push(format!("--continue-text={text}"));
    }
    if let Some(when) = args.continue_when.filter(|_| !blank) {
        words.push(format!(
            "--continue-when={}",
            match when {
                When::Limited => "limited",
                When::Always => "always",
                When::Never => "never",
            }
        ));
    }
    words.extend(args.exclude.iter().map(|pane| format!("--exclude={pane}")));
    words.push("--json".to_owned());
    words
}

#[derive(clap::Parser)]
#[command(name = "swap")]
struct SwapCli {
    #[command(flatten)]
    args: Args,
}

/// The canonical `swap … --json` argv for `words` (a `muxa swap` command
/// line without the program name), checked by the same parser the CLI uses
/// so conflicting options fail here, before any process is spawned.
pub(crate) fn checked_argv(words: &[String]) -> std::result::Result<Vec<String>, String> {
    use clap::Parser as _;
    let cli =
        SwapCli::try_parse_from(std::iter::once("swap".to_owned()).chain(words.iter().cloned()))
            .map_err(|error| {
                error
                    .to_string()
                    .lines()
                    .next()
                    .unwrap_or_default()
                    .to_owned()
            })?;
    Ok(remote_argv(&cli.args))
}

/// `muxa swap --host`: the host's own `muxa swap` does the work, through
/// the Fleet transport, and its report is printed here.
async fn run_remote(client: &Client, args: &Args, host: &str) -> Result<()> {
    let output = client
        .work_command(Some(host), &remote_argv(args), None)
        .await
        .with_context(|| format!("running the swap on {host}"))?;
    let Ok(outcomes) = serde_json::from_str::<Vec<SwapOutcome>>(&output.stdout) else {
        bail!(
            "{host}: swap failed (exit {}): {}",
            output.exit_code,
            output.stderr.trim()
        );
    };
    if args.json {
        println!("{}", serde_json::to_string_pretty(&outcomes)?);
    } else {
        print!("{}", render(&outcomes));
    }
    if outcomes.iter().any(|o| o.status == SwapStatus::Failed) {
        bail!("some agents on {host} could not be swapped");
    }
    Ok(())
}

fn render(outcomes: &[SwapOutcome]) -> String {
    if outcomes.is_empty() {
        return "nothing to swap\n".to_owned();
    }
    let mut out = String::new();
    for outcome in outcomes {
        let pane = outcome.pane.as_deref().unwrap_or("-");
        let moved = match (&outcome.from, &outcome.to) {
            (from, Some(to)) => format!("{} -> {to}", from.as_deref().unwrap_or("(bare)")),
            (Some(from), None) => from.clone(),
            (None, None) => String::new(),
        };
        let _ = writeln!(
            out,
            "{:<8} {pane:<10} {:<11} {moved}",
            outcome.status.to_string(),
            outcome.kind.to_string(),
        );
        if let Some(command) = &outcome.command {
            let _ = writeln!(out, "         $ {command}");
        }
        if !outcome.note.is_empty() {
            let _ = writeln!(out, "         {}", outcome.note);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    #[derive(Parser)]
    struct Cli {
        #[command(flatten)]
        args: Args,
    }

    fn parse(argv: &[&str]) -> SwapRequest {
        let cli = Cli::parse_from(std::iter::once("swap").chain(argv.iter().copied()));
        request(&cli.args, &SwapConfig::default())
    }

    fn request_from(args: &Args, config: &SwapConfig) -> SwapRequest {
        request(args, config)
    }

    #[test]
    fn host_runs_the_same_swap_there_as_one_word_per_value() {
        let cli = Cli::parse_from([
            "swap",
            "%5",
            "--force",
            "--provider",
            "claude",
            "--to",
            "work@claude",
            "--continue-text",
            "keep going",
            "--continue-when",
            "always",
            "--host",
            "rtzr",
        ]);
        let argv = remote_argv(&cli.args);
        assert_eq!(
            argv,
            [
                "swap",
                "%5",
                "--force",
                "--provider=claude",
                "--to=work@claude",
                "--continue-text=keep going",
                "--continue-when=always",
                "--json"
            ]
        );
        muxa::work_control::validate_swap_argv(&argv).unwrap();
    }

    #[test]
    fn conflicting_options_fail_before_anything_is_sent() {
        let words = |w: &[&str]| w.iter().map(|w| (*w).to_owned()).collect::<Vec<_>>();
        assert!(checked_argv(&words(&["%5", "--all"])).is_err());
        assert!(checked_argv(&words(&["--to", "x", "--spread"])).is_err());
        assert_eq!(
            checked_argv(&words(&["--all", "--exclude", "%9"])).unwrap(),
            ["swap", "--all", "--to=auto", "--exclude=%9", "--json"]
        );
    }

    #[test]
    fn no_target_means_the_rate_limited_agents() {
        let request = parse(&[]);
        assert_eq!(request.scope, SwapScope::Limited);
        assert_eq!(request.to, AccountChoice::Auto);
        assert_eq!(request.continue_when, ContinueWhen::Limited);
    }

    #[test]
    fn targets_all_and_account_flags_map_onto_the_request() {
        let request = parse(&["%5", "--to", "jelly@claude", "--bare", "wrap"]);
        assert_eq!(request.scope, SwapScope::Targets(vec!["%5".into()]));
        assert_eq!(request.to, AccountChoice::Named("jelly@claude".into()));
        assert_eq!(request.bare, BareMode::Wrap);

        let request = parse(&["--all", "--spread", "--force", "--provider", "codex"]);
        assert_eq!(request.scope, SwapScope::All);
        assert_eq!(request.to, AccountChoice::Spread);
        assert!(request.force);
        assert_eq!(request.provider, Some(AgentKind::Codex));
    }

    /// Naming the wording keeps the configured policy: it used to switch it
    /// to `always`, which woke every idle agent a swap touched.
    #[test]
    fn the_wording_and_who_gets_it_are_set_separately() {
        let request = parse(&["--continue-text", "keep going"]);
        assert_eq!(request.continue_text.as_deref(), Some("keep going"));
        assert_eq!(request.continue_when, ContinueWhen::Limited);

        let request = parse(&["--continue-when", "always"]);
        assert_eq!(
            request.continue_text,
            SwapRequest::new(SwapScope::Limited, &SwapConfig::default()).continue_text
        );
        assert_eq!(request.continue_when, ContinueWhen::Always);

        let request = parse(&["--continue-text", "go", "--continue-when", "never"]);
        assert_eq!(request.continue_when, ContinueWhen::Never);
        assert_eq!(parse(&["--no-continue"]).continue_when, ContinueWhen::Never);

        // A configured policy survives a wording-only override.
        let config = SwapConfig {
            continue_when: ContinueWhen::Never,
            ..SwapConfig::default()
        };
        let cli = Cli::parse_from(["swap", "--continue-text", "go"]);
        assert_eq!(
            request_from(&cli.args, &config).continue_when,
            ContinueWhen::Never
        );
        let config = SwapConfig {
            continue_when: ContinueWhen::Always,
            ..SwapConfig::default()
        };
        assert_eq!(
            request_from(&cli.args, &config).continue_when,
            ContinueWhen::Always
        );
    }

    /// The old spelling is the wording only now, and a remote run carries
    /// the new spellings a host too old to know them refuses.
    #[test]
    fn the_deprecated_continue_no_longer_means_every_agent() {
        let request = parse(&["--continue", "keep going"]);
        assert_eq!(request.continue_text.as_deref(), Some("keep going"));
        assert_eq!(request.continue_when, ContinueWhen::Limited);

        let words = |w: &[&str]| w.iter().map(|w| (*w).to_owned()).collect::<Vec<_>>();
        assert_eq!(
            checked_argv(&words(&["--continue", "go"])).unwrap(),
            ["swap", "--to=auto", "--continue-text=go", "--json"]
        );
        assert!(checked_argv(&words(&["--continue", "a", "--continue-text", "b"])).is_err());
        assert!(checked_argv(&words(&["--no-continue", "--continue-when", "always"])).is_err());
        assert!(checked_argv(&words(&["--continue-when", "sometimes"])).is_err());
        let argv = checked_argv(&words(&[
            "--continue-text",
            "go",
            "--continue-when",
            "limited",
        ]))
        .unwrap();
        muxa::work_control::validate_swap_argv(&argv).unwrap();
        // No policy named: none is sent, so the host keeps its own default.
        assert!(!checked_argv(&words(&["--continue-text", "go"]))
            .unwrap()
            .iter()
            .any(|word| word.starts_with("--continue-when")));
    }

    /// A blank wording sends nothing, here and on a Fleet host alike.
    #[test]
    fn a_blank_wording_sends_nothing_here_or_on_a_host() {
        assert_eq!(parse(&["--continue-text", " "]).continue_text, None);
        let words = |w: &[&str]| w.iter().map(|w| (*w).to_owned()).collect::<Vec<_>>();
        for blank in [
            words(&["--continue-text", " "]),
            words(&["--continue", ""]),
            words(&["--continue-text", "", "--continue-when", "always"]),
        ] {
            let argv = checked_argv(&blank).unwrap();
            assert!(argv.contains(&"--no-continue".to_owned()), "{argv:?}");
            assert!(
                !argv.iter().any(|w| w.starts_with("--continue")),
                "{argv:?}"
            );
            muxa::work_control::validate_swap_argv(&argv).unwrap();
        }
    }

    #[test]
    fn report_shows_the_move_and_the_typed_line() {
        let rendered = render(&[SwapOutcome {
            pane: Some("%5".into()),
            agent_session_id: "sid".into(),
            kind: AgentKind::ClaudeCode,
            from: Some("e-ed@claude".into()),
            to: Some("jelly@claude".into()),
            status: SwapStatus::Planned,
            command: Some("aas e jelly@claude --resume sid".into()),
            note: String::new(),
            continue_reason: None,
            session_id_source: None,
            conversation_verified: None,
            registered: None,
        }]);
        assert!(
            rendered.contains("e-ed@claude -> jelly@claude"),
            "{rendered}"
        );
        assert!(
            rendered.contains("$ aas e jelly@claude --resume sid"),
            "{rendered}"
        );
        assert_eq!(render(&[]), "nothing to swap\n");
    }
}
