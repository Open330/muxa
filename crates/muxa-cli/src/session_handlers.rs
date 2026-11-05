use super::{Client, Context, Duration, Path, PathBuf, Result, Write};

pub(super) async fn cmd_run(
    client: &Client,
    socket_path: &Path,
    name: Option<String>,
    cwd: Option<PathBuf>,
    detach: bool,
    command: Vec<String>,
) -> Result<()> {
    let (cols, rows) = crossterm::terminal::size().unwrap_or((120, 40));
    let Some((program, args)) = command.split_first() else {
        anyhow::bail!("missing command");
    };
    let session = client
        .spawn_session(muxa::SpawnSession {
            command: program.clone(),
            args: args.to_vec(),
            env: caller_env(socket_path),
            cwd: Some(cwd.unwrap_or(std::env::current_dir()?)),
            name,
            cols: Some(cols),
            rows: Some(rows),
        })
        .await
        .context("spawning muxa session")?;
    println!(
        "muxa: started {} ({})",
        session
            .display_name
            .as_deref()
            .unwrap_or(session.id.as_str()),
        session.id
    );
    if !detach {
        attach_session(client, &session.id).await?;
    }
    Ok(())
}

pub(crate) fn caller_env(socket_path: &Path) -> Vec<(String, String)> {
    let mut env = std::env::vars_os()
        .filter_map(|(key, value)| Some((key.into_string().ok()?, value.into_string().ok()?)))
        .collect::<Vec<_>>();
    match env.iter_mut().find(|(key, _)| key == "MUXA_SOCKET") {
        Some((_, value)) if value.is_empty() => {
            *value = socket_path.display().to_string();
        }
        Some(_) => {}
        None => env.push(("MUXA_SOCKET".into(), socket_path.display().to_string())),
    }
    env
}

pub(super) async fn cmd_attach(client: &Client, session: &str) -> Result<()> {
    let sessions = client.list_sessions().await?;
    let id = sessions
        .iter()
        .find(|s| s.id == session || s.display_name.as_deref() == Some(session))
        .map_or_else(|| session.to_string(), |s| s.id.clone());
    attach_session(client, &id).await
}

pub(super) async fn cmd_detach(client: &Client, session: &str) -> Result<()> {
    client.set_session_attached(session, false).await?;
    println!("muxa: detached {session}");
    Ok(())
}

// `Option` because the Windows arm can fail to find its own process.
#[cfg(unix)]
#[allow(clippy::unnecessary_wraps)]
pub(super) fn parent_pid() -> Option<u32> {
    Some(std::os::unix::process::parent_id())
}

/// Windows has no `getppid`; the process table records the parent instead.
#[cfg(not(unix))]
pub(super) fn parent_pid() -> Option<u32> {
    use sysinfo::{Pid, ProcessRefreshKind, ProcessesToUpdate, System};
    let own = Pid::from_u32(std::process::id());
    let mut system = System::new();
    system.refresh_processes_specifics(
        ProcessesToUpdate::Some(&[own]),
        false,
        ProcessRefreshKind::nothing(),
    );
    system.process(own)?.parent().map(Pid::as_u32)
}

pub(super) async fn cmd_register(
    client: &Client,
    name: String,
    pid: Option<u32>,
    cwd: Option<String>,
    pane: Option<String>,
) -> Result<()> {
    // Default to the calling shell so `muxa proc register --name X` from a pane
    // tracks that shell without the user hunting for a pid.
    let pid = pid.or_else(parent_pid);
    client
        .register(&name, pid, cwd.as_deref(), pane.as_deref(), None)
        .await?;
    match pid {
        Some(p) => println!("muxa: registered {name} (pid {p})"),
        None => println!("muxa: registered {name}"),
    }
    Ok(())
}

pub(super) async fn cmd_zellij_plugin_snapshot(client: &Client, json: &str) -> Result<()> {
    let panes: Vec<muxa::tmux::PaneInfo> =
        serde_json::from_str(json).context("parsing zellij pane snapshot")?;
    client
        .push_pane_snapshot(&panes)
        .await
        .context("pushing zellij pane snapshot")?;
    Ok(())
}

pub(super) struct RawModeGuard;

impl RawModeGuard {
    pub(super) fn enter() -> Result<Self> {
        crossterm::terminal::enable_raw_mode()?;
        if let Err(error) =
            crossterm::execute!(std::io::stdout(), crossterm::event::EnableBracketedPaste)
        {
            // `execute!` may have written the enable sequence before a later
            // flush error. Best-effort reversal keeps a failed attach from
            // leaving the parent terminal in paste mode without a guard.
            let _ = crossterm::execute!(std::io::stdout(), crossterm::event::DisableBracketedPaste);
            let _ = crossterm::terminal::disable_raw_mode();
            return Err(error.into());
        }
        Ok(Self)
    }
}

impl Drop for RawModeGuard {
    fn drop(&mut self) {
        let _ = crossterm::execute!(std::io::stdout(), crossterm::event::DisableBracketedPaste);
        let _ = crossterm::terminal::disable_raw_mode();
    }
}

pub(super) async fn attach_session(client: &Client, session_id: &str) -> Result<()> {
    let _guard = RawModeGuard::enter()?;
    let attachment_id = format!("muxa-cli:{}", std::process::id());
    client
        .set_session_client_attached(session_id, &attachment_id, true)
        .await?;
    let result = attach_session_loop(client, session_id).await;
    let detach_result = client
        .set_session_client_attached(session_id, &attachment_id, false)
        .await;
    match (result, detach_result) {
        (Err(e), _) => Err(e),
        (Ok(()), Err(e)) => Err(e).context("detaching muxa session"),
        (Ok(()), Ok(())) => Ok(()),
    }
}

pub(super) async fn attach_session_loop(client: &Client, session_id: &str) -> Result<()> {
    use crossterm::event::{Event, KeyCode, KeyEventKind};

    let mut stdout = std::io::stdout().lock();
    let mut offset = 0_u64;
    let mut detach_armed = false;

    loop {
        let output = client.read_session(session_id, offset).await?;
        let output_bytes = output
            .data_bytes()
            .context("decoding byte-safe PTY output")?;
        if !output_bytes.is_empty() {
            stdout.write_all(&output_bytes)?;
            stdout.flush()?;
            offset = output.next_offset;
        }
        if output.exited {
            break;
        }

        while crossterm::event::poll(Duration::ZERO)? {
            match crossterm::event::read()? {
                Event::Paste(text) => {
                    // A detach prefix applies only to the immediately
                    // following key, never across a whole paste event.
                    detach_armed = false;
                    client
                        .write_session(session_id, &bracketed_paste_input(&text))
                        .await?;
                }
                Event::Key(key) if key.kind == KeyEventKind::Press => {
                    if detach_armed {
                        detach_armed = false;
                        if matches!(key.code, KeyCode::Char('d' | 'D')) {
                            return Ok(());
                        }
                        client.write_session(session_id, "\u{1d}").await?;
                    }
                    if is_detach_prefix(key) {
                        detach_armed = true;
                    } else if let Some(input) = key_to_pty_input(key) {
                        client.write_session(session_id, &input).await?;
                    }
                }
                Event::Resize(cols, rows) => {
                    let _ = client.resize_session(session_id, cols, rows).await;
                }
                _ => {}
            }
        }

        tokio::time::sleep(Duration::from_millis(30)).await;
    }

    Ok(())
}

/// Recreate the framing that crossterm strips from an `Event::Paste` before
/// relaying it to the child PTY. Keeping the payload bracketed prevents a
/// multiline paste from being executed one line at a time by an interactive
/// shell. The parent terminal reports line feeds; PTY input expects carriage
/// returns, with CRLF normalized first so it does not become a doubled CR.
pub(super) fn bracketed_paste_input(text: &str) -> String {
    const START: &str = "\x1b[200~";
    const END: &str = "\x1b[201~";

    // Clipboard text can contain terminal controls. If an end marker reaches
    // the child unchanged, readline leaves paste mode early and processes the
    // remainder as ordinary keystrokes, including carriage returns that execute
    // commands. Remove markers while streaming so overlapping inputs cannot
    // reveal a fresh marker after an earlier one is deleted.
    let mut defanged = Vec::with_capacity(text.len());
    for byte in text.bytes() {
        defanged.push(byte);
        if defanged.ends_with(START.as_bytes()) || defanged.ends_with(END.as_bytes()) {
            defanged.truncate(defanged.len() - START.len());
        }
    }
    let defanged = String::from_utf8(defanged).expect("removing ASCII preserves UTF-8");
    let normalized = defanged.replace("\r\n", "\n").replace('\n', "\r");
    format!("{START}{normalized}{END}")
}

pub(super) fn is_detach_prefix(key: crossterm::event::KeyEvent) -> bool {
    key.modifiers
        .contains(crossterm::event::KeyModifiers::CONTROL)
        && matches!(key.code, crossterm::event::KeyCode::Char(']'))
}

pub(super) fn key_to_pty_input(key: crossterm::event::KeyEvent) -> Option<String> {
    use crossterm::event::{KeyCode, KeyModifiers};

    // The parent terminal owns platform shortcuts. If one still reaches
    // crossterm, dropping it is safer than turning Cmd+V into a literal `v`.
    if key.modifiers.contains(KeyModifiers::SUPER) {
        return None;
    }

    Some(match key.code {
        KeyCode::Char(c) if key.modifiers.contains(KeyModifiers::CONTROL) => {
            let lower = c.to_ascii_lowercase();
            if lower.is_ascii_lowercase() {
                char::from((lower as u8) - b'a' + 1).to_string()
            } else {
                return None;
            }
        }
        KeyCode::Char(c) => c.to_string(),
        KeyCode::Enter => "\r".into(),
        KeyCode::Backspace => "\u{7f}".into(),
        KeyCode::Tab => "\t".into(),
        KeyCode::Esc => "\u{1b}".into(),
        KeyCode::Left => "\u{1b}[D".into(),
        KeyCode::Right => "\u{1b}[C".into(),
        KeyCode::Up => "\u{1b}[A".into(),
        KeyCode::Down => "\u{1b}[B".into(),
        KeyCode::Home => "\u{1b}[H".into(),
        KeyCode::End => "\u{1b}[F".into(),
        KeyCode::Delete => "\u{1b}[3~".into(),
        _ => return None,
    })
}
