use super::{Agent, AgentKind, Client, Context, Duration, OffsetDateTime, Result};

/// Parse a short human duration (`45s`, `30m`, `1h`, `2d`, or a bare
/// seconds count) into whole seconds. Used by `tend registry prune --older-than`.
pub(super) fn parse_duration_secs(s: &str) -> Result<u64> {
    let s = s.trim();
    anyhow::ensure!(!s.is_empty(), "empty duration");
    let (num, mult) = match s.chars().last() {
        Some('d') => (&s[..s.len() - 1], 86_400),
        Some('h') => (&s[..s.len() - 1], 3_600),
        Some('m') => (&s[..s.len() - 1], 60),
        Some('s') => (&s[..s.len() - 1], 1),
        _ => (s, 1),
    };
    let val: u64 = num
        .trim()
        .parse()
        .with_context(|| format!("invalid duration: {s:?} (try 30m, 1h, 24h)"))?;
    Ok(val * mult)
}

/// Delete orphan agent rows (no pane, surface, or pid) via the daemon.
/// Previews the count from a snapshot, confirms (unless `--yes`), then asks
/// the daemon to prune. tmux sessions are never affected — only registry rows.
pub(super) async fn cmd_prune(
    client: &Client,
    older_than: &str,
    all: bool,
    yes: bool,
) -> Result<()> {
    let max_age_secs = if all {
        0
    } else {
        parse_duration_secs(older_than)?
    };
    let now = OffsetDateTime::now_utc();
    let agents = client.snapshot().await?;
    let sessions = client.list_sessions().await?;
    let is_orphan = |a: &Agent| {
        let missing_surface = a.surface.as_ref().is_none_or(|surface| {
            surface.kind == tend::event::SurfaceKind::Pty
                && !sessions.iter().any(|session| session.id == surface.id)
        });
        a.kind != AgentKind::Task && a.pane.is_none() && missing_surface && a.pid.is_none()
    };
    let candidates = agents
        .iter()
        .filter(|a| {
            is_orphan(a)
                && (now - a.last_activity_at).whole_seconds()
                    >= i64::try_from(max_age_secs).unwrap_or(i64::MAX)
        })
        .count();
    if candidates == 0 {
        println!("No orphan agent rows to prune.");
        return Ok(());
    }
    if !yes {
        let scope = if all {
            "all ages".to_string()
        } else {
            format!("idle ≥ {older_than}")
        };
        let proceed = cliclack::confirm(format!(
            "Prune {candidates} orphan agent row(s) ({scope})? tmux sessions are not affected."
        ))
        .initial_value(false)
        .interact()
        .unwrap_or(false);
        if !proceed {
            println!("Aborted.");
            return Ok(());
        }
    }
    let pruned = client.prune(Duration::from_secs(max_age_secs)).await?;
    println!("Pruned {pruned} orphan agent row(s).");
    Ok(())
}

pub(super) async fn cmd_forget_agent(client: &Client, session_id: &str, yes: bool) -> Result<()> {
    let agent = client.snapshot().await?.into_iter()
        .find(|agent| agent.session_id == session_id)
        .with_context(|| format!("No agent with exact session id {session_id:?}. Use tend status --json to find agent_session_id."))?;
    if !yes {
        let proceed = cliclack::confirm(format!(
            "Forget {} session {} ({})? Only its registry row is removed; later activity can register it again.",
            agent.kind,
            agent.session_id,
            agent.pane.as_deref().unwrap_or("no pane"),
        ))
        .initial_value(false)
        .interact()?;
        if !proceed {
            println!("Aborted.");
            return Ok(());
        }
    }
    if client.forget_agent(session_id).await? {
        println!("Forgot agent session {session_id}.");
    } else {
        println!("Agent session {session_id} was already absent.");
    }
    Ok(())
}
