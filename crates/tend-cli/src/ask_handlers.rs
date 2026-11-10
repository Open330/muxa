use super::{
    ask_progress, AskCmd, AskProviderCmd, Client, Context, Duration, IsTerminal, Read, Result,
};

/// Refuse with a pointed message when the running daemon predates a
/// request kind, instead of the generic "unknown kind" it would answer.
pub(super) async fn require_capability(
    client: &Client,
    capability: &str,
    what: &str,
) -> Result<()> {
    let hello = client.hello(Duration::from_secs(2)).await?;
    anyhow::ensure!(
        hello.capabilities.iter().any(|value| value == capability),
        "the running tendd is too old for {what}; restart it from this tend version"
    );
    Ok(())
}

pub(super) async fn cmd_ask_admin(client: &Client, action: AskCmd) -> Result<()> {
    require_capability(client, "ask_providers_v1", "ask providers").await?;
    match action {
        AskCmd::Providers { json } => {
            let providers = client.ask_providers().await?;
            if json {
                println!("{}", serde_json::to_string_pretty(&providers)?);
            } else {
                print!("{}", render_ask_providers(&providers));
            }
        }
        AskCmd::Provider {
            action:
                AskProviderCmd::Set {
                    id,
                    title,
                    model,
                    api_key_env,
                    executable,
                    clear_title,
                    clear_model,
                    clear_api_key_env,
                    clear_executable,
                    json,
                },
        } => {
            // Only the flags given travel: an omitted key stays as it is.
            let key = |set: Option<String>, clear: bool| {
                if clear {
                    Some(None)
                } else {
                    set.map(Some)
                }
            };
            let edit = tend::ask::AskProviderEdit {
                title: key(title, clear_title),
                model: key(model, clear_model),
                api_key_env: key(api_key_env, clear_api_key_env),
                executable: key(executable, clear_executable),
            };
            let providers = client.ask_provider_configure(&id, &edit).await?;
            report_provider_change(&providers, &id, json)?;
        }
        AskCmd::Provider {
            action:
                AskProviderCmd::Add {
                    id,
                    engine,
                    title,
                    model,
                    api_key_env,
                    executable,
                    json,
                },
        } => {
            let request = tend::ask::AskProviderAdd {
                id: id.clone(),
                engine: engine.label().to_string(),
                title,
                model,
                api_key_env,
                executable,
            };
            let providers = client.ask_provider_add(&request).await?;
            report_provider_change(&providers, &id, json)?;
        }
        AskCmd::Provider {
            action: AskProviderCmd::Use { id, json },
        } => {
            require_capability(client, "ask_provider_default_v1", "ask provider use").await?;
            let providers = client.ask_provider_default(&id).await?;
            if json {
                println!("{}", serde_json::to_string_pretty(&providers)?);
            } else {
                let selected = providers
                    .iter()
                    .find(|provider| provider.selected)
                    .context("the daemon reported no selected provider")?;
                println!(
                    "{}: now the default ask provider ({} on {}); saved as [ask] agent",
                    selected.id, selected.title, selected.engine
                );
            }
        }
        AskCmd::Provider {
            action: AskProviderCmd::Remove { id, json },
        } => {
            let providers = client.ask_provider_remove(&id).await?;
            if json {
                println!("{}", serde_json::to_string_pretty(&providers)?);
            } else if providers.iter().any(|provider| provider.id == id) {
                println!("{id}: settings cleared; the built-in provider stands again");
            } else {
                println!("{id}: removed");
            }
        }
    }
    Ok(())
}

/// One line describing the provider a write just touched, or the whole
/// list as JSON.
pub(super) fn report_provider_change(
    providers: &[tend::ask::AskProviderInfo],
    id: &str,
    json: bool,
) -> Result<()> {
    if json {
        println!("{}", serde_json::to_string_pretty(providers)?);
        return Ok(());
    }
    let changed = providers
        .iter()
        .find(|provider| provider.id == id)
        .context("the daemon did not echo the provider back")?;
    println!(
        "{}: {} on {}, model {}, key from {}{}",
        changed.id,
        changed.title,
        changed.engine,
        changed.model.as_deref().unwrap_or("(provider default)"),
        changed.credential_env,
        if changed.credential_present {
            " (present)"
        } else {
            ""
        }
    );
    Ok(())
}

pub(super) fn render_ask_providers(providers: &[tend::ask::AskProviderInfo]) -> String {
    use std::fmt::Write as _;
    let mut out = String::new();
    for provider in providers {
        let via = match provider.kind {
            tend::ask::AskProviderKind::Cli => {
                provider.executable.clone().unwrap_or_else(|| "cli".into())
            }
            tend::ask::AskProviderKind::Api => "api".into(),
        };
        let credential = if provider.credential_env.is_empty() {
            // The apple engine: the model belongs to the signed-in Mac.
            "no key needed".to_string()
        } else if provider.credential_present {
            format!("{} present", provider.credential_env)
        } else if provider.credential_required {
            format!("{} missing", provider.credential_env)
        } else {
            format!("{} optional", provider.credential_env)
        };
        let _ = writeln!(
            out,
            "{} {:<16} {:<18} {:<10} {:<10} {:<18} {}",
            if provider.selected { "*" } else { " " },
            provider.id,
            provider.title,
            provider.engine,
            via,
            provider.model.as_deref().unwrap_or("-"),
            credential
        );
    }
    out
}

/// Flags of one `tend ask <prompt>` invocation.
#[allow(clippy::struct_excessive_bools)] // independent CLI switches
pub(super) struct AskOptions {
    pub(super) agent: Option<String>,
    pub(super) api_key_stdin: bool,
    pub(super) detach: bool,
    pub(super) new: bool,
    pub(super) json: bool,
    pub(super) timeout_secs: u64,
}

pub(super) async fn cmd_ask(client: &Client, prompt: String, options: AskOptions) -> Result<()> {
    let AskOptions {
        agent,
        api_key_stdin,
        detach,
        new,
        json,
        timeout_secs,
    } = options;
    let selected = client.ask_agent(agent.as_deref()).await?;
    if new {
        client.ask_reset().await?;
    }
    let api_key = if api_key_stdin {
        anyhow::ensure!(
            !std::io::stdin().is_terminal(),
            "--api-key-stdin requires a pipe; refusing to echo a secret in the terminal"
        );
        let mut value = String::new();
        std::io::stdin().read_to_string(&mut value)?;
        let value = value.trim().to_string();
        anyhow::ensure!(!value.is_empty(), "stdin did not contain an API key");
        let hello = client.hello(Duration::from_secs(2)).await?;
        anyhow::ensure!(
            hello
                .capabilities
                .iter()
                .any(|value| value == "ask_one_turn_credential_v1"),
            "the running tendd is too old for one-turn API keys; restart it from this tend version"
        );
        Some(value)
    } else {
        None
    };
    let pending = client
        .ask_send_with_credential(&prompt, Some(&selected), api_key.as_deref())
        .await?;
    if detach {
        if json {
            println!("{}", serde_json::to_string_pretty(&pending)?);
        } else {
            println!("queued {} via {}", pending.id, pending.agent);
        }
        return Ok(());
    }

    wait_for_ask(client, &pending.id, json, timeout_secs).await
}

pub(super) async fn wait_for_ask(
    client: &Client,
    id: &str,
    json: bool,
    timeout_secs: u64,
) -> Result<()> {
    let deadline =
        tokio::time::Instant::now() + Duration::from_secs(timeout_secs.clamp(1, 24 * 60 * 60));
    // Subscribe before reading the snapshot, so a finishing turn cannot fall
    // between our read and wait. Older/remote daemons retain bounded polling.
    let mut updates = client.ask_subscribe().await.ok();
    let mut shown = String::new();
    loop {
        let entry = client
            .ask_list_filtered(Some(id), None)
            .await?
            .into_iter()
            .find(|entry| entry.id == id)
            .context("the queued Ask disappeared from durable history")?;
        match entry.status {
            tend::ask::AskStatus::Running => {
                if !json && std::io::stderr().is_terminal() && entry.answer != shown {
                    let added = ask_progress::delta(&shown, &entry.answer).trim();
                    if !added.is_empty() {
                        eprintln!("{added}");
                    }
                    shown.clone_from(&entry.answer);
                }
                anyhow::ensure!(
                    tokio::time::Instant::now() < deadline,
                    "timed out waiting for {}; the Ask remains queued in tendd history",
                    entry.id
                );
                if let Some(stream) = updates.as_mut() {
                    let refresh =
                        deadline.min(tokio::time::Instant::now() + Duration::from_secs(30));
                    if matches!(
                        tokio::time::timeout_at(refresh, stream.recv()).await,
                        Ok(Ok(None) | Err(_))
                    ) {
                        updates = None;
                    }
                } else {
                    tokio::time::sleep(
                        Duration::from_secs(1)
                            .min(deadline.saturating_duration_since(tokio::time::Instant::now())),
                    )
                    .await;
                }
            }
            tend::ask::AskStatus::Answered => {
                if json {
                    println!("{}", serde_json::to_string_pretty(&entry)?);
                } else {
                    println!("{}", entry.answer);
                }
                return Ok(());
            }
            tend::ask::AskStatus::Failed => {
                if json {
                    println!("{}", serde_json::to_string_pretty(&entry)?);
                    anyhow::bail!("Ask {} failed", entry.id);
                }
                anyhow::bail!(
                    "{}",
                    entry.error.as_deref().unwrap_or("headless provider failed")
                );
            }
        }
    }
}
