use super::*;
use crate::ask_handlers::render_ask_providers;
use crate::hook_handlers::alias_target;
use crate::mailbox_handlers::is_operator_list_origin;
use crate::mailbox_handlers::render_inbox_request;
use crate::mailbox_handlers::render_message_list;
use crate::mailbox_handlers::render_reply_output;
use crate::mailbox_handlers::CollaborationListFilters;
use crate::navigation::dispatch_kind;
use crate::navigation::resolve_jump_session;
use crate::navigation::rmux_client_from_context;
use crate::navigation::session_target;
use crate::navigation::window_target;
use crate::session_handlers::bracketed_paste_input;
use crate::session_handlers::key_to_pty_input;
use crate::status_handlers::attention_segment;
use crate::status_handlers::pane_inventory_lines;
use crate::status_handlers::render_status_table;
use crate::status_handlers::render_status_tree;
use crate::status_handlers::state_icon_ascii;
use crate::status_handlers::state_icon_unicode;
use crate::status_handlers::status_json;

#[test]
fn only_untracked_unpinned_terminal_lists_use_operator_identity() {
    use tend::ipc::RuntimeError;
    let mut origin = CollaborationOrigin {
        exact_session: false,
        pane: "rmux:%71".into(),
        socket: Some("/tmp/rmux/default".into()),
        console: false,
        agent_session_id: None,
    };
    let untracked = RuntimeError::Daemon("collaboration origin is not a hook-correlated tracked pane agent: rmux:%71; trigger an agent event or restart the agent".into());
    assert!(is_operator_list_origin(&origin, &untracked));
    origin.agent_session_id = Some("missing-agent".into());
    assert!(!is_operator_list_origin(&origin, &untracked));
    origin.agent_session_id = None;
    assert!(!is_operator_list_origin(
        &origin,
        &RuntimeError::Daemon("permission denied".into())
    ));
    assert!(!is_operator_list_origin(
        &origin,
        &RuntimeError::Timeout(std::time::Duration::from_secs(1))
    ));
}

fn dashboard(argv: &[&str]) -> dashboard_cmd::Args {
    let mut full = vec!["tend", "dashboard"];
    full.extend_from_slice(argv);
    let Some(Cmd::Dashboard(parsed)) = Args::try_parse_from(full).unwrap().cmd else {
        panic!("not a dashboard invocation")
    };
    parsed
}

#[test]
fn dashboard_flags_carry_over_to_the_work_board() {
    let defaults = dashboard(&[]);
    assert_eq!(
        defaults.board_sort(),
        tend::config::WatchBoardSort::Attention
    );
    assert_eq!(defaults.board_since().unwrap(), "today");
    let chosen = dashboard(&[
        "--sort",
        "name",
        "--since",
        "7d",
        "--include-paneless",
        "--theme",
        "mono",
    ]);
    assert_eq!(chosen.board_sort(), tend::config::WatchBoardSort::Name);
    assert_eq!(chosen.board_since().unwrap(), "7d");
    assert!(chosen.include_paneless());
    assert!(matches!(chosen.theme(), Some(ThemeArg::Mono)));
    assert_eq!(
        dashboard(&["--sort", "active"]).board_sort(),
        tend::config::WatchBoardSort::Active
    );
    assert!(dashboard(&["--since", "yesterdayish"])
        .board_since()
        .is_err());
    assert!(Args::try_parse_from(["tend", "dashboard", "--legacy"]).is_err());
}

#[test]
fn message_list_defaults_are_bounded_and_full_output_is_explicit() {
    let parsed = Args::try_parse_from(["tend", "msg", "list", "--json"]).unwrap();
    let Cmd::Msg {
        action:
            MsgCmd::List {
                limit,
                full,
                request_id,
                scope,
                ..
            },
    } = parsed.cmd.expect("explicit subcommand")
    else {
        panic!("expected list")
    };
    assert_eq!(scope, "auto");
    assert_eq!(limit, Some(20));
    assert!(!full);
    assert!(request_id.is_none());
    let mut request = collaboration_output::fixture();
    request.body = "private original instructions".into();
    let compact: serde_json::Value =
        serde_json::from_str(&render_message_list(&[request.clone()], false).unwrap()).unwrap();
    assert_eq!(compact[0]["id"], request.id);
    assert_eq!(compact[0]["created_at"], "2026-10-14T00:00:00Z");
    assert!(compact[0].get("body").is_none());
    assert!(compact[0].get("reply").is_none());
    assert!(compact[0].get("provenance").is_none());
    let legacy: serde_json::Value =
        serde_json::from_str(&render_message_list(&[request], true).unwrap()).unwrap();
    assert_eq!(legacy[0]["body"], "private original instructions");
    assert!(
        Args::try_parse_from(["tend", "msg", "list", "--request-id", "req-1", "--full"]).is_ok()
    );
}

#[test]
fn explicit_zero_limit_preserves_complete_filtered_history() {
    let request = collaboration_output::fixture();
    let requests = vec![request; 25];
    let default_page = CollaborationListFilters {
        limit: Some(20),
        ..Default::default()
    }
    .apply(requests.clone());
    assert_eq!(default_page.len(), 20);
    let all = CollaborationListFilters {
        limit: Some(0),
        ..Default::default()
    }
    .apply(requests);
    assert_eq!(all.len(), 25);
}

#[test]
fn reload_scopes_preserve_legacy_mux_and_reject_cross_scope_options() {
    for argv in [
        vec!["tend", "reload", "--yes"],
        vec!["tend", "reload", "mux", "--yes"],
        vec!["tend", "reload", "mcp", "--dry-run", "--json"],
        vec!["tend", "reload", "mcp", "--all"],
        vec!["tend", "reload", "mcp", "--codex-home", "/profiles/account"],
    ] {
        assert!(Args::try_parse_from(&argv).is_ok(), "rejected {argv:?}");
    }
    for argv in [
        vec!["tend", "reload", "--yes", "mcp"],
        vec!["tend", "reload", "mcp", "--layout-only"],
        vec![
            "tend",
            "reload",
            "mcp",
            "--all",
            "--codex-home",
            "/profiles/account",
        ],
        vec!["tend", "reload", "mcp", "--timeout-secs", "0"],
    ] {
        assert!(Args::try_parse_from(&argv).is_err(), "accepted {argv:?}");
    }
}

#[test]
fn reply_output_preserves_json_escapes_and_direct_body() {
    let mut request = collaboration_output::fixture();
    let body = "pending\nquote: \" \\ path\n한글";
    request.status = RequestStatus::Failed;
    request.reply = Some(tend::collaboration::CollaborationReply {
        report: None,
        status: RequestStatus::Failed,
        body: body.into(),
        artifacts: vec![],
        at: time::OffsetDateTime::UNIX_EPOCH,
    });
    let output = render_reply_output(&request, false, false).unwrap();
    let value: serde_json::Value = serde_json::from_str(&output).unwrap();
    assert_eq!(value["reply"]["body"], body);
    assert_eq!(value["final_reply_received"], true);
    assert_eq!(value["reported_status"], "failed");
    assert_eq!(render_reply_output(&request, true, false).unwrap(), body);
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(
            &render_reply_output(&request, false, true).unwrap()
        )
        .unwrap(),
        serde_json::json!(request)
    );
}

#[test]
fn reply_timeout_does_not_close_or_fail_work() {
    let request = collaboration_output::fixture();
    let output = render_reply_output(&request, false, false).unwrap();
    let value: serde_json::Value = serde_json::from_str(&output).unwrap();
    assert_eq!(value["reason"], "timeout");
    assert_eq!(value["delivery_state"], "claimed");
    assert_eq!(value["final_reply_received"], false);
    assert!(value.get("reported_status").is_none());
    assert!(render_reply_output(&request, true, false).is_err());
}

#[test]
fn inbox_text_preserves_contract_and_wait_accepts_timeout_alias() {
    let mut request = collaboration_output::fixture();
    request.paths = vec!["src/permitted.rs".into()];
    request.artifacts = vec!["review.txt".into()];
    request.expects_reply = false;
    let text = render_inbox_request(&request);
    for required in [
        "src/permitted.rs",
        "review.txt",
        "expects_reply=false",
        &request.body,
    ] {
        assert!(text.contains(required), "{text}");
    }
    let args =
        Args::try_parse_from(["tend", "msg", "wait", "req-test", "--timeout", "60"]).unwrap();
    assert!(matches!(
        args.cmd,
        Some(Cmd::Msg {
            action: MsgCmd::Wait {
                timeout_secs: 60,
                ..
            }
        })
    ));
}

#[test]
fn message_wait_cli_supports_direct_body_file_and_legacy_full_record() {
    let args = Args::try_parse_from([
        "tend",
        "msg",
        "wait",
        "req-test",
        "--body",
        "--output",
        "/tmp/reply.txt",
    ])
    .unwrap();
    assert!(matches!(
        args.cmd.expect("explicit subcommand"),
        Cmd::Msg {
            action: MsgCmd::Wait {
                body: true,
                output: Some(_),
                ..
            },
            ..
        }
    ));
    assert!(Args::try_parse_from(["tend", "msg", "wait", "req-test", "--full"]).is_ok());
    assert!(Args::try_parse_from(["tend", "msg", "wait", "req-test", "--body", "--full"]).is_err());
}

/// rmux exports the tmux compatibility variables alongside its own, and
/// the CLI once read `$TMUX_PANE` directly — yielding a bare `%118` where
/// hooks stamp `rmux:%118`, so the origin matched no tracked agent and
/// `tend peers`/`msg`/`identity` refused on every host but tmux.
///
/// The env cannot be set in-process (racy under parallel test threads, and
/// the workspace forbids the unsafe setter), so the test re-runs itself as
/// a child with the environment it needs.
#[test]
fn cli_collaboration_origin_preserves_rmux_identity() {
    const PROBE: &str = "TEND_TEST_CLI_ORIGIN_PROBE";
    if std::env::var_os(PROBE).is_some() {
        let origin = mcp::current_collaboration_origin().unwrap();
        assert_eq!(origin.pane, "rmux:%118");
        assert_eq!(
            origin.socket.as_deref(),
            Some("/tmp/tend-origin-test/default")
        );
        assert!(!origin.console);
        return;
    }
    // Isolate environment changes from parallel tests. rmux exports both
    // native and tmux compatibility variables; native identity must win.
    let output = std::process::Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "tests::cli_collaboration_origin_preserves_rmux_identity",
            "--nocapture",
        ])
        .env(PROBE, "1")
        .env("RMUX", "/tmp/tend-origin-test/default,42,1")
        .env("RMUX_PANE", "%118")
        .env("TMUX", "/tmp/tend-origin-test/default,42,1")
        .env("TMUX_PANE", "%118")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

use tend::collaboration::{CollaborationRequest, RoomId};
use tend::AgentKind;
use time::macros::datetime;
use unicode_width::UnicodeWidthStr;

#[test]
fn attention_command_requires_a_human_action_not_a_notice() {
    let args = Args::try_parse_from([
        "tend",
        "msg",
        "attention",
        "Approve staging?",
        "--action",
        "approval",
        "--json",
    ])
    .unwrap();
    assert!(
        matches!(args.cmd, Some(Cmd::Msg { action: MsgCmd::Attention { action, json: true, .. } }) if action == "approval")
    );
    assert!(
        Args::try_parse_from(["tend", "msg", "attention", "Test", "--action", "notice"]).is_err()
    );
}

#[test]
fn registry_forget_requires_exact_session_argument() {
    let args =
        Args::try_parse_from(["tend", "registry", "forget", "exact-session", "--yes"]).unwrap();
    assert!(matches!(args.cmd, Some(Cmd::Registry {
            action: RegistryCmd::Forget { session_id, yes: true }
        }) if session_id == "exact-session"));
    assert!(Args::try_parse_from(["tend", "registry", "forget"]).is_err());
}

#[tokio::test]
async fn registry_forget_rejects_prefix_and_removes_selected_record() {
    let dir = tempfile::tempdir().unwrap();
    let socket = dir.path().join("forget-cli.sock");
    let store = tend::Store::shared();
    let server = tend::ipc::Server::new(socket.clone(), store.clone());
    let (shutdown, rx) = tokio::sync::broadcast::channel(1);
    let handle = tokio::spawn(async move { server.run(rx).await.unwrap() });
    for _ in 0..50 {
        if socket.exists() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    let client = Client::new(socket);
    for name in ["ghost-exact", "ghost-other"] {
        client
            .ingest(&tend::AgentEvent::Started {
                id: tend::AgentId {
                    kind: AgentKind::Codex,
                    session_id: name.into(),
                    surface: None,
                    pane: Some(format!("cmux:{name}")),
                    tmux_socket: Some("/tmp/cmux.sock".into()),
                    cwd: None,
                },
                at: OffsetDateTime::now_utc(),
            })
            .await
            .unwrap();
    }
    let err = cmd_forget_agent(&client, "ghost", true).await.unwrap_err();
    assert!(err.to_string().contains("exact session id"));
    assert_eq!(store.snapshot().await.len(), 2);
    cmd_forget_agent(&client, "ghost-exact", true)
        .await
        .unwrap();
    assert!(store.by_session("ghost-exact").await.is_none());
    assert!(store.by_session("ghost-other").await.is_some());
    shutdown.send(()).unwrap();
    handle.await.unwrap();
}

#[tokio::test]
async fn prune_command_includes_missing_native_session_in_preview() {
    let dir = tempfile::tempdir().unwrap();
    let socket = dir.path().join("prune-native.sock");
    let store = tend::Store::shared();
    let server = tend::ipc::Server::new(socket.clone(), store.clone());
    let (shutdown, rx) = tokio::sync::broadcast::channel(1);
    let handle = tokio::spawn(async move { server.run(rx).await.unwrap() });
    for _ in 0..50 {
        if socket.exists() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    let client = Client::new(socket);
    client
        .ingest(&tend::AgentEvent::Started {
            id: tend::AgentId {
                kind: AgentKind::Codex,
                session_id: "eng-7651-ghost".into(),
                surface: Some(tend::SurfaceRef {
                    kind: tend::SurfaceKind::Pty,
                    id: "pty:missing".into(),
                    workspace: None,
                }),
                pane: None,
                tmux_socket: None,
                cwd: None,
            },
            at: OffsetDateTime::now_utc() - time::Duration::days(2),
        })
        .await
        .unwrap();
    cmd_prune(&client, "1h", false, true).await.unwrap();
    assert!(store.by_session("eng-7651-ghost").await.is_none());
    shutdown.send(()).unwrap();
    handle.await.unwrap();
}

#[test]
fn snapshot_restore_reload_and_upgrade_each_carry_their_own_help() {
    use clap::CommandFactory;
    let command = Args::command();
    let about = |name: &str| {
        command
            .find_subcommand(name)
            .and_then(|sub| sub.get_about())
            .map(ToString::to_string)
            .unwrap_or_default()
    };
    assert!(about("snapshot").starts_with("Capture this multiplexer"));
    assert!(about("restore").starts_with("Rebuild a workspace"));
    assert!(about("reload").starts_with("Reload MCP"));
    assert!(about("upgrade").starts_with("Update tend from the source repo"));
}

#[test]
fn rmux_client_resolution_uses_the_invoking_session_without_guessing() {
    let listing = "/dev/pts/1\t$1\n/dev/pts/2\t$2\n";
    assert_eq!(
        rmux_client_from_context(listing, "/tmp/socket,10,2").as_deref(),
        Some("/dev/pts/2")
    );
    assert_eq!(rmux_client_from_context(listing, "/tmp/socket,10,3"), None);
    assert_eq!(
        rmux_client_from_context("a\t$1\nb\t$1", "/tmp/socket,10,1"),
        None
    );
}

/// The harness supplies a disposable server with an attached client and
/// a target pane in a different session/window. No user server is touched.
#[test]
#[ignore = "requires an isolated rmux server and attached client"]
fn live_rmux_jump_switches_client_session_and_window() {
    use tend::PaneBackend;
    let socket = std::env::var("TEND_RMUX_TEST_ENDPOINT").unwrap();
    let pane_id = std::env::var("TEND_RMUX_TEST_PANE").unwrap();
    let client = std::env::var("TEND_RMUX_TEST_CLIENT").unwrap();
    assert_eq!(
        tend::backend::rmux::endpoint_from_env().as_deref(),
        Some(socket.as_str())
    );
    CALLER_CLIENT.set(client.clone()).unwrap();
    let backend = tend::RmuxBackend::with_endpoint(&socket);
    let pane = backend.resolve_pane(&pane_id).unwrap();
    let key = tend::PaneKey::from_pane(tend::HostKind::Rmux, &pane);
    jump_to_topology_pane(&key);
    let clients = || {
        backend
            .capture_control(&["list-clients", "-F", "#{client_name}\t#{session_id}"])
            .unwrap()
    };
    let session_for = |name: &str| {
        clients()
            .lines()
            .find_map(|line| {
                let (client, session) = line.split_once('\t')?;
                (client == name).then(|| session.to_string())
            })
            .unwrap()
    };
    let view = session_for(&client);
    let control = mux_control::Control::Endpoint(key.window.session.endpoint.clone());
    {
        let attach_view = tmux_work::prepare_attach_view(&control, &pane.session_id).unwrap();
        assert_ne!(attach_view.id, pane.session_id);
        let linked = backend
            .capture_control(&["list-windows", "-t", &attach_view.id, "-F", "#{window_id}"])
            .unwrap();
        assert!(linked.lines().any(|window| window == pane.window_id));
    }
    if let Ok(other) = std::env::var("TEND_RMUX_TEST_OTHER_CLIENT") {
        assert_ne!(view, pane.session_id, "a busy session needs a private view");
        assert_eq!(session_for(&other), pane.session_id);
        let original = backend
            .capture_control(&[
                "display-message",
                "-p",
                "-t",
                &pane.session_id,
                "#{window_id}",
            ])
            .unwrap();
        assert_ne!(
            original.trim(),
            pane.window_id,
            "another terminal's selected window changed"
        );
    }
    let output = backend
        .capture_control(&[
            "display-message",
            "-p",
            "-t",
            &view,
            "#{window_id}.#{pane_id}",
        ])
        .unwrap();
    assert_eq!(
        output.trim(),
        format!(
            "{}.{}",
            pane.window_id,
            pane_id.strip_prefix("rmux:").unwrap_or(&pane_id)
        )
    );
    // Repeated jumps reuse this terminal's view even though topology
    // reports the canonical source session for the shared window.
    jump_to_topology_pane(&key);
    assert_eq!(session_for(&client), view);
    let sessions = backend
        .capture_control(&["list-sessions", "-F", "#{session_name}"])
        .unwrap();
    assert_eq!(
        sessions
            .lines()
            .filter(|name| name.contains("~view~"))
            .count(),
        usize::from(view != pane.session_id)
    );
}

#[test]
fn ask_command_supports_headless_provider_and_stdin_key() {
    let args = Args::try_parse_from([
        "tend",
        "ask",
        "summarize this window",
        "--agent",
        "codex",
        "--api-key-stdin",
        "--detach",
        "--json",
    ])
    .unwrap();
    let Cmd::Ask {
        action,
        prompt,
        agent,
        api_key_stdin,
        detach,
        json,
        ..
    } = args.cmd.expect("explicit subcommand")
    else {
        panic!("expected ask command");
    };
    assert!(action.is_none());
    assert_eq!(prompt.as_deref(), Some("summarize this window"));
    assert_eq!(agent.as_deref(), Some("codex"));
    assert!(api_key_stdin);
    assert!(detach);
    assert!(json);
}

#[test]
fn ask_new_starts_a_fresh_conversation_only_when_asked() {
    let fresh = Args::try_parse_from(["tend", "ask", "--new", "start over"]).unwrap();
    let Cmd::Ask { new, prompt, .. } = fresh.cmd.expect("explicit subcommand") else {
        panic!("expected ask command");
    };
    assert!(new);
    assert_eq!(prompt.as_deref(), Some("start over"));
    let resumed = Args::try_parse_from(["tend", "ask", "continue"]).unwrap();
    assert!(matches!(
        resumed.cmd.expect("explicit subcommand"),
        Cmd::Ask { new: false, .. }
    ));
}

#[test]
fn the_provider_table_names_the_engine_behind_each_instance() {
    let mut providers = std::collections::BTreeMap::new();
    providers.insert(
        "anthropic-work".to_string(),
        tend::config::AskProviderConfig {
            engine: Some("anthropic".into()),
            title: Some("Anthropic (work)".into()),
            api_key_env: Some("WORK_ANTHROPIC_KEY".into()),
            ..tend::config::AskProviderConfig::default()
        },
    );
    let env = |name: &str| (name == "WORK_ANTHROPIC_KEY").then(|| "k".to_string());
    let rendered = render_ask_providers(&tend::ask::provider_infos(
        &providers,
        "anthropic-work",
        env,
    ));
    let mut lines = rendered.lines();
    let work = lines.next().expect("the composed instance leads");
    assert!(work.starts_with('*'), "{work}");
    assert!(work.contains("anthropic-work"), "{work}");
    assert!(work.contains("Anthropic (work)"), "{work}");
    // The engine is its own column, so two instances of one engine are
    // told apart by id while still showing what drives them.
    assert!(work.contains("anthropic"), "{work}");
    assert!(work.contains("ANTHROPIC_API_KEY present"), "{work}");
    // Every built-in still follows it, unselected.
    let rest: Vec<&str> = lines.collect();
    assert_eq!(rest.len(), tend::ask::supported_agents().len());
    assert!(rest.iter().all(|line| line.starts_with(' ')), "{rest:?}");
    let claude = rest.first().expect("claude leads the built-ins");
    assert!(claude.contains("Claude Code"), "{claude}");
    assert!(claude.contains("ANTHROPIC_API_KEY optional"), "{claude}");
}

#[allow(clippy::too_many_lines)] // one parse assertion per ask subcommand
#[test]
fn ask_accepts_every_provider_id_and_its_admin_subcommands() {
    // Built-in ids and composed ones alike: `--agent` names a provider
    // instance, so it cannot be a closed value set.
    for id in tend::ask::supported_agents()
        .iter()
        .chain(["anthropic-work", "openai_personal"].iter())
    {
        let args = Args::try_parse_from(["tend", "ask", "--agent", id, "hello"]).unwrap();
        let Cmd::Ask { agent, .. } = args.cmd.expect("explicit subcommand") else {
            panic!("expected ask command");
        };
        assert_eq!(agent.as_deref(), Some(*id));
    }

    let args = Args::try_parse_from(["tend", "ask", "providers", "--json"]).unwrap();
    let Cmd::Ask {
        action: Some(AskCmd::Providers { json }),
        prompt: None,
        ..
    } = args.cmd.expect("explicit subcommand")
    else {
        panic!("expected ask providers");
    };
    assert!(json);

    let args = Args::try_parse_from([
        "tend",
        "ask",
        "provider",
        "set",
        "anthropic",
        "--model",
        "claude-opus-5",
        "--clear-api-key-env",
    ])
    .unwrap();
    let Cmd::Ask {
        action:
            Some(AskCmd::Provider {
                action:
                    AskProviderCmd::Set {
                        id,
                        model,
                        api_key_env,
                        clear_model,
                        clear_api_key_env,
                        ..
                    },
            }),
        ..
    } = args.cmd.expect("explicit subcommand")
    else {
        panic!("expected ask provider set");
    };
    assert_eq!(id, "anthropic");
    assert_eq!(model.as_deref(), Some("claude-opus-5"));
    assert_eq!(api_key_env, None);
    assert!(!clear_model);
    assert!(clear_api_key_env);
    let args = Args::try_parse_from(["tend", "ask", "provider", "use", "codex"]).unwrap();
    assert!(matches!(
        args.cmd.expect("explicit subcommand"),
        Cmd::Ask {
            action: Some(AskCmd::Provider {
                action: AskProviderCmd::Use { ref id, json: false },
            }),
            ..
        } if id == "codex"
    ));
    // Setting and clearing the same key at once is a contradiction.
    assert!(Args::try_parse_from([
        "tend",
        "ask",
        "provider",
        "set",
        "openai",
        "--model",
        "gpt-5",
        "--clear-model"
    ])
    .is_err());
    // Composing a provider: the id is free text, the engine is not.
    let args = Args::try_parse_from([
        "tend",
        "ask",
        "provider",
        "add",
        "anthropic-work",
        "--engine",
        "anthropic",
        "--title",
        "Anthropic (work)",
        "--api-key-env",
        "WORK_ANTHROPIC_KEY",
        "--executable",
        "/opt/homebrew/bin/claude",
    ])
    .unwrap();
    let Cmd::Ask {
        action:
            Some(AskCmd::Provider {
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
            }),
        ..
    } = args.cmd.expect("explicit subcommand")
    else {
        panic!("expected ask provider add");
    };
    assert_eq!(id, "anthropic-work");
    assert_eq!(engine.label(), "anthropic");
    assert_eq!(title.as_deref(), Some("Anthropic (work)"));
    assert_eq!(model, None);
    assert_eq!(api_key_env.as_deref(), Some("WORK_ANTHROPIC_KEY"));
    assert_eq!(executable.as_deref(), Some("/opt/homebrew/bin/claude"));
    assert!(!json);
    // The engine is required, and only the five tend ships are accepted.
    assert!(Args::try_parse_from(["tend", "ask", "provider", "add", "mine"]).is_err());
    assert!(
        Args::try_parse_from(["tend", "ask", "provider", "add", "mine", "--engine", "bard"])
            .is_err()
    );

    let args = Args::try_parse_from([
        "tend",
        "ask",
        "provider",
        "remove",
        "anthropic-work",
        "--json",
    ])
    .unwrap();
    let Cmd::Ask {
        action:
            Some(AskCmd::Provider {
                action: AskProviderCmd::Remove { id, json },
            }),
        ..
    } = args.cmd.expect("explicit subcommand")
    else {
        panic!("expected ask provider remove");
    };
    assert_eq!(id, "anthropic-work");
    assert!(json);

    // A question that is not a subcommand name still asks.
    let args = Args::try_parse_from(["tend", "ask", "what providers exist?"]).unwrap();
    let Cmd::Ask {
        action: None,
        prompt,
        ..
    } = args.cmd.expect("explicit subcommand")
    else {
        panic!("expected a plain ask");
    };
    assert_eq!(prompt.as_deref(), Some("what providers exist?"));
}

#[test]
fn work_compose_cli_takes_a_description_agent_and_current_draft() {
    let args = Args::try_parse_from([
        "tend",
        "work",
        "compose",
        "implementer in claude, reviewer in codex after it",
        "--agent",
        "codex",
        "--current",
        "-",
        "--json",
    ])
    .unwrap();
    let Cmd::Work {
        action: WorkCmd::Compose(compose),
    } = args.cmd.expect("explicit subcommand")
    else {
        panic!("expected work compose");
    };
    assert_eq!(
        compose.description,
        "implementer in claude, reviewer in codex after it"
    );
    assert_eq!(compose.agent.as_deref(), Some("codex"));
    assert_eq!(compose.current.as_deref(), Some(std::path::Path::new("-")));
    assert!(compose.json);
    assert!(Args::try_parse_from(["tend", "work", "compose"]).is_err());
}

fn started(kind: AgentKind, pane: Option<&str>) -> tend::event::AgentEvent {
    tend::event::AgentEvent::Started {
        id: tend::event::AgentId {
            kind,
            session_id: "s".into(),
            surface: None,
            pane: pane.map(ToString::to_string),
            tmux_socket: None,
            cwd: None,
        },
        at: time::OffsetDateTime::now_utc(),
    }
}

fn collaboration_participant(pane: &str, window: &str) -> Participant {
    Participant {
        launch_purpose: tend::collaboration::LaunchPurpose::Primary,
        draining: false,
        agent_kind: AgentKind::Codex,
        agent_session_id: format!("agent-{pane}"),
        pane: pane.to_string(),
        socket: Some("test".into()),
        room: RoomId {
            host: "local".into(),
            socket: Some("test".into()),
            window_id: format!("@{window}"),
        },
        tmux_session_id: Some("$1".into()),
        tmux_session_name: Some("webapp".into()),
        window_name: Some(window.into()),
        state: AgentState::Idle,
        cwd: None,
        alias: None,
        roles: Vec::new(),
        console: false,
    }
}

fn collaboration_request(
    id: &str,
    at: OffsetDateTime,
    window: &str,
    work_id: Option<&str>,
    thread_id: Option<&str>,
    kind: RequestKind,
    status: RequestStatus,
) -> CollaborationRequest {
    CollaborationRequest {
        turn_offer: None,
        user_authorized: false,
        task_contract: None,
        attempt: None,
        supersedes_request_id: None,
        superseded_by: None,
        late_replies: Vec::new(),
        dispatch_id: None,
        source_node_id: None,
        delegation_parent: None,
        interruption: None,
        retirement: None,
        human_action: None,
        initiator: None,
        updates: Vec::new(),
        id: id.into(),
        from: collaboration_participant("%1", window),
        to: collaboration_participant("%2", window),
        provenance: None,
        kind,
        body: format!("body {id}"),
        expects_reply: true,
        notify: None,
        work_mode: WorkMode::ReadOnly,
        thread_id: thread_id.map(str::to_string),
        parent_request_id: None,
        workspace_id: Some("tend".into()),
        work_id: work_id.map(str::to_string),
        run_id: None,
        paths: Vec::new(),
        artifacts: Vec::new(),
        links: Vec::new(),
        status,
        created_at: at,
        claimed_at: None,
        wake_delivery: None,
        notified_at: None,
        reply_notified_at: None,
        reply_read_at: None,
        reply_wake: None,
        reply: None,
        withdrew_reply_wakes: Vec::new(),
        rebound_from: Vec::new(),
        to_slot: None,
    }
}

#[test]
fn message_progress_read_requires_an_exact_request() {
    for extra in [vec![], vec!["--full"], vec!["--limit", "10"]] {
        let mut args = vec!["tend", "msg", "list", "--after-update", "2"];
        if !extra.is_empty() {
            args.extend(["--request-id", "req-1"]);
        }
        args.extend(extra);
        assert!(Args::try_parse_from(args).is_err());
    }
    let cli = Args::try_parse_from([
        "tend",
        "msg",
        "list",
        "--request-id",
        "req-1",
        "--after-update",
        "2",
        "--json",
    ])
    .unwrap();
    assert!(matches!(
        cli.cmd,
        Some(Cmd::Msg {
            action: MsgCmd::List {
                request_id: Some(_),
                after_update: Some(2),
                full: false,
                ..
            }
        })
    ));
}

#[test]
fn message_notify_cli_accepts_shorthand_and_explicit_booleans() {
    for (extra, expected) in [
        (vec![], None),
        (vec!["--notify", "true"], Some(true)),
        (vec!["--notify", "false"], Some(false)),
        (vec!["--notify"], Some(true)),
        (vec!["--notify=true"], Some(true)),
        (vec!["--notify=false"], Some(false)),
        (vec!["--notify", "--json"], Some(true)),
    ] {
        let mut args = vec![
            "tend",
            "msg",
            "send",
            "@peer",
            "checkpoint",
            "--kind",
            "notice",
        ];
        args.extend(extra);
        let cli = Args::try_parse_from(args).unwrap();
        let Cmd::Msg {
            action: MsgCmd::Send { notify, .. },
        } = cli.cmd.expect("explicit subcommand")
        else {
            panic!("expected message send");
        };
        assert_eq!(notify, expected);
    }
    assert!(Args::try_parse_from([
        "tend",
        "msg",
        "send",
        "@peer",
        "checkpoint",
        "--notify",
        "maybe"
    ])
    .is_err());
}

#[test]
fn attached_session_paste_is_reframed_and_normalizes_newlines() {
    assert_eq!(
        bracketed_paste_input("first\nsecond\r\nthird"),
        "\x1b[200~first\rsecond\rthird\x1b[201~"
    );
}

#[test]
fn attached_session_paste_cannot_close_its_own_brackets() {
    let framed =
        bracketed_paste_input("safe\x1b[201~\r\nnext\x1b[200~\x1b\x1b[201~[201~\r\ncommand");
    assert!(framed.starts_with("\x1b[200~"));
    assert!(framed.ends_with("\x1b[201~"));
    assert_eq!(framed.matches("\x1b[200~").count(), 1);
    assert_eq!(framed.matches("\x1b[201~").count(), 1);
    assert_eq!(&framed[6..framed.len() - 6], "safe\rnext\rcommand");
}

#[test]
fn attached_session_drops_super_shortcuts() {
    let key = crossterm::event::KeyEvent::new(
        crossterm::event::KeyCode::Char('v'),
        crossterm::event::KeyModifiers::SUPER,
    );
    assert_eq!(key_to_pty_input(key), None);
}

#[test]
fn a_paneless_start_names_nothing() {
    // `run_hook` reports `pane: None` for a tend-owned PTY surface on
    // purpose: that agent's shell inherited `$TMUX_PANE` from whatever
    // terminal asked for the PTY, and that pane owns nothing. Reaching
    // for the environment here would name the outer pane after the
    // runtime running inside the PTY.
    assert_eq!(alias_target(&started(AgentKind::ClaudeCode, None)), None);
}

#[test]
fn only_a_session_start_names_a_pane() {
    assert_eq!(
        alias_target(&started(AgentKind::ClaudeCode, Some("%7"))),
        Some(("%7".to_string(), "claude")),
    );
    // Everything else on the hook path fires per tool call.
    let tool = tend::event::AgentEvent::ToolStarted {
        id: tend::event::AgentId {
            kind: AgentKind::ClaudeCode,
            session_id: "s".into(),
            surface: None,
            pane: Some("%7".into()),
            tmux_socket: None,
            cwd: None,
        },
        tool: "Bash".into(),
        subagent: None,
        command: None,
        at: time::OffsetDateTime::now_utc(),
    };
    assert_eq!(alias_target(&tool), None);
}

#[test]
fn pid_tracked_and_unknown_rows_name_nothing() {
    // A `Task` row is a subagent under a pane, not a pane of its own.
    assert_eq!(alias_target(&started(AgentKind::Task, Some("%7"))), None);
    assert_eq!(alias_target(&started(AgentKind::Unknown, Some("%7"))), None);
}

#[test]
fn dispatch_kind_prefers_pane_namespace_over_fallback() {
    use tend::HostKind;
    // Namespaced ids dispatch to their own host regardless of the
    // process-global fallback — a herdr row jumps via herdr even when
    // the shell is tmux-primary, and vice versa.
    assert_eq!(dispatch_kind("%3", HostKind::Herdr), HostKind::Tmux);
    assert_eq!(dispatch_kind("rmux:%3", HostKind::Tmux), HostKind::Rmux);
    assert_eq!(dispatch_kind("herdr:abc", HostKind::Tmux), HostKind::Herdr);
    assert_eq!(dispatch_kind("zellij:7", HostKind::Tmux), HostKind::Zellij);
    // Unrecognized ids fall back to the process-global host.
    assert_eq!(dispatch_kind("legacy-id", HostKind::Tmux), HostKind::Tmux);
    assert_eq!(dispatch_kind("legacy-id", HostKind::Herdr), HostKind::Herdr);
}

#[test]
fn window_target_qualifies_the_window_with_its_session() {
    // The whole point: a window addressed together with its session
    // cannot be resolved into a *different* session of the same group.
    assert_eq!(window_target(Some("$1"), "@4", "%9"), "$1:@4");
}

#[test]
fn window_target_falls_back_to_the_pane_id() {
    // No session, no window, or an empty id from an older PANE_FMT: the
    // pane id is what every jump used before, so degrade to it rather
    // than emit a malformed target like `:@4` that tmux would reject.
    assert_eq!(window_target(None, "@4", "%9"), "%9");
    assert_eq!(window_target(Some("$1"), "", "%9"), "%9");
    assert_eq!(window_target(Some(""), "@4", "%9"), "%9");
}

#[test]
fn jump_stays_in_the_asking_client_session_without_probing() {
    // The ordinary same-session jump: the ids already match, so the
    // membership probe — a tmux round trip on a keypress path — must not
    // run at all.
    let answer = resolve_jump_session(Some("$0".into()), "$0", |_| {
        panic!("probed tmux for a session we already know matches")
    });
    assert_eq!(answer.as_deref(), Some("$0"));
}

#[test]
fn jump_stays_in_the_asking_client_session_when_the_window_is_linked() {
    // The session-group case, and the whole point of the change: the pane
    // is recorded under `$0`, but the asking client sits in the grouped
    // sibling `$1` where that window is linked too. Answering `$1` keeps
    // this terminal in its own session, so `$0` — and whoever is looking
    // at it — never moves.
    let answer = resolve_jump_session(Some("$1".into()), "$0", |session| {
        assert_eq!(session, "$1");
        true
    });
    assert_eq!(answer.as_deref(), Some("$1"));
}

#[test]
fn jump_crosses_to_the_pane_session_when_the_window_is_not_linked() {
    // A genuine cross-session jump. The pane's own session is a definite
    // destination; tmux would otherwise choose one from client activity.
    let answer = resolve_jump_session(Some("$1".into()), "$0", |_| false);
    assert_eq!(answer.as_deref(), Some("$0"));
}

#[test]
fn jump_falls_back_when_the_client_session_is_unknown() {
    // No caller client, or a client that detached mid-call: the pane's
    // session still beats letting tmux guess.
    assert_eq!(
        resolve_jump_session(None, "$0", |_| unreachable!()).as_deref(),
        Some("$0")
    );
    assert_eq!(
        resolve_jump_session(Some(String::new()), "$0", |_| unreachable!()).as_deref(),
        Some("$0")
    );
    // Neither known — `window_target` degrades to the bare pane id.
    assert_eq!(resolve_jump_session(None, "", |_| unreachable!()), None);
}

#[test]
fn session_target_prefers_the_stable_id_over_the_name() {
    // `webapp` matches `webapp-set` by prefix; `$3` matches nothing else.
    assert_eq!(session_target("$3", "webapp"), "$3");
    // Only a row with no id at all falls back to the ambiguous name.
    assert_eq!(session_target("", "webapp"), "webapp");
}

fn agent(session_id: &str, pane: Option<&str>, state: AgentState, prompt: &str) -> Agent {
    Agent {
        ask_origin: None,
        tmux_socket: None,
        tmux_session: None,
        kind: AgentKind::ClaudeCode,
        session_id: session_id.into(),
        surface: None,
        pane: pane.map(str::to_string),
        pid: None,
        workload: tend::WorkloadSummary::default(),
        subagents: Vec::new(),
        cwd: None,
        state,
        last_prompt: Some(prompt.into()),
        last_prompt_at: None,
        last_response: None,
        recap: None,
        ai_title: None,
        pending_command: None,
        last_notification: None,
        model: Some("claude-sonnet-very-long-model-name".into()),
        context_used_pct: None,
        cost_usd: None,
        rate_limit_5h_pct: None,
        rate_limit_5h_resets_at: None,
        rate_limit_7d_pct: None,
        rate_limit_7d_resets_at: None,
        rate_limited_until: None,
        rate_limit_scope: None,
        rate_limit_source: None,
        started_at: datetime!(2026-04-24 12:00:00 UTC),
        last_activity_at: datetime!(2026-04-24 11:55:00 UTC),
        state_entered_at: datetime!(2026-04-24 11:55:00 UTC),
        last_prompt_origin: None,
        last_event_at: None,
        last_response_at: None,
        awaiting_reply: None,
    }
}

fn pane(id: &str, session: &str) -> tend::tmux::PaneInfo {
    tend::tmux::PaneInfo {
        session_group: None,
        agent_program: None,
        worker_launch_id: None,
        agent_role: None,
        agent_alias: None,
        workspace_id: None,
        work_id: None,
        socket: None,
        pane_id: id.into(),
        session_id: String::new(),
        session: session.into(),
        window_id: String::new(),
        window_name: String::new(),
        window_index: "12".into(),
        pane_index: "3".into(),
        tty: "/dev/pts/0".into(),
        current_command: "claude".into(),
        title: String::new(),
        pane_pid: 0,
        current_path: String::new(),
    }
}

#[test]
fn watch_sort_cli_aliases_parse_to_expected_keys() {
    for (raw, expected) in [
        ("name", vec![WatchSortKey::Name, WatchSortKey::Activity]),
        ("act", vec![WatchSortKey::Activity]),
        ("dur", vec![WatchSortKey::Duration]),
        ("st", vec![WatchSortKey::State, WatchSortKey::Activity]),
        ("pane_id", vec![WatchSortKey::PaneId]),
    ] {
        let args = Args::try_parse_from(["tend", "watch", "--sort", raw]).unwrap();
        let Cmd::Watch {
            sort: Some(sort), ..
        } = args.cmd.expect("explicit subcommand")
        else {
            panic!("expected watch sort arg");
        };
        assert_eq!(sort.keys(), expected);
    }
}

#[test]
fn watch_rejects_legacy_view_and_sort_vocabulary() {
    for args in [
        ["tend", "watch", "--view", "work"],
        ["tend", "watch", "--view", "swarm"],
        ["tend", "watch", "--sort", "workspace"],
        ["tend", "watch", "--sort", "workspace-time"],
    ] {
        assert!(Args::try_parse_from(args).is_err(), "accepted {args:?}");
    }
    assert!(Args::try_parse_from(["tend", "watch", "--layout", "swarm"]).is_ok());
    assert!(Args::try_parse_from(["tend", "watch", "--layout", "sequence"]).is_err());
    assert!(Args::try_parse_from(["tend", "watch", "--collab-layout", "tree"]).is_err());
}

#[test]
fn watch_collaboration_layout_is_independent_from_topology_layout() {
    let args = Args::try_parse_from([
        "tend",
        "watch",
        "--screen",
        "collab",
        "--layout",
        "tree",
        "--collab-layout",
        "sequence",
    ])
    .unwrap();
    let Cmd::Watch {
        layout: Some(WatchLayoutArg::Tree),
        collab_layout: Some(collab_layout),
        ..
    } = args.cmd.expect("explicit subcommand")
    else {
        panic!("expected independent topology and collaboration layouts");
    };
    assert_eq!(Some(collab_layout), watch::parse_collab_layout("sequence"));
}

#[test]
fn message_skill_cli_is_discoverable() {
    let args = Args::try_parse_from([
        "tend",
        "skill",
        "add",
        "agent-review",
        "ask codex to review our changes",
    ])
    .unwrap();
    assert!(matches!(
        args.cmd.expect("explicit subcommand"),
        Cmd::Skill(_)
    ));
    assert!(Args::try_parse_from(["tend", "skill", "list"]).is_ok());
    assert!(Args::try_parse_from(["tend", "skill", "remove", "agent-review"]).is_ok());
}

#[test]
fn message_list_cli_parses_history_filters_and_pagination() {
    let args = Args::try_parse_from([
        "tend",
        "msg",
        "list",
        "--scope",
        "all",
        "--since",
        "7d",
        "--work",
        "ENG-7345",
        "--workspace",
        "tend",
        "--thread",
        "review-cycle",
        "--kind",
        "review",
        "--status",
        "completed",
        "--room",
        "ENG-7345",
        "--offset",
        "20",
        "--limit",
        "10",
        "--json",
    ])
    .unwrap();
    let Cmd::Msg {
        action:
            MsgCmd::List {
                scope,
                since,
                work,
                workspace,
                thread,
                kind,
                status,
                window,
                offset,
                limit,
                json,
                ..
            },
    } = args.cmd.expect("explicit subcommand")
    else {
        panic!("expected msg list");
    };
    assert_eq!(scope, "all");
    assert_eq!(since.as_deref(), Some("7d"));
    assert_eq!(work.as_deref(), Some("ENG-7345"));
    assert_eq!(workspace.as_deref(), Some("tend"));
    assert_eq!(thread.as_deref(), Some("review-cycle"));
    assert_eq!(kind.as_deref(), Some("review"));
    assert_eq!(status.as_deref(), Some("completed"));
    assert_eq!(window.as_deref(), Some("ENG-7345"));
    assert_eq!(offset, 20);
    assert_eq!(limit, Some(10));
    assert!(json);
}

#[test]
fn message_send_cli_parses_correlation_and_artifact_metadata() {
    let args = Args::try_parse_from([
        "tend",
        "msg",
        "send",
        "@reviewer",
        "review this",
        "--thread",
        "review-cycle",
        "--parent",
        "req-parent",
        "--work",
        "ENG-7345",
        "--workspace",
        "tend",
        "--run",
        "run-2",
        "--artifact",
        "commit:d4bf2aa53",
        "--link",
        "https://example.test/review",
    ])
    .unwrap();
    let Cmd::Msg {
        action:
            MsgCmd::Send {
                thread,
                parent,
                work,
                workspace,
                run,
                artifacts,
                links,
                ..
            },
    } = args.cmd.expect("explicit subcommand")
    else {
        panic!("expected msg send");
    };
    assert_eq!(thread.as_deref(), Some("review-cycle"));
    assert_eq!(parent.as_deref(), Some("req-parent"));
    assert_eq!(work.as_deref(), Some("ENG-7345"));
    assert_eq!(workspace.as_deref(), Some("tend"));
    assert_eq!(run.as_deref(), Some("run-2"));
    assert_eq!(artifacts, ["commit:d4bf2aa53"]);
    assert_eq!(links, ["https://example.test/review"]);
}

#[test]
fn message_history_filters_before_paginating_and_keeps_newest_order() {
    let requests = vec![
        collaboration_request(
            "newest",
            datetime!(2026-08-30 12:00:00 UTC),
            "ENG-7345",
            Some("ENG-7345"),
            Some("review-cycle"),
            RequestKind::Review,
            RequestStatus::Completed,
        ),
        collaboration_request(
            "wrong-kind",
            datetime!(2026-08-30 11:00:00 UTC),
            "ENG-7345",
            Some("ENG-7345"),
            Some("review-cycle"),
            RequestKind::Notice,
            RequestStatus::Completed,
        ),
        collaboration_request(
            "second-match",
            datetime!(2026-08-30 10:00:00 UTC),
            "ENG-7345",
            Some("ENG-7345"),
            Some("review-cycle"),
            RequestKind::Review,
            RequestStatus::Completed,
        ),
        collaboration_request(
            "too-old",
            datetime!(2026-08-29 09:00:00 UTC),
            "ENG-7345",
            Some("ENG-7345"),
            Some("review-cycle"),
            RequestKind::Review,
            RequestStatus::Completed,
        ),
    ];
    let filters = CollaborationListFilters {
        range: Some(time_range::TimeRange {
            label: "test".into(),
            since_at: Some(datetime!(2026-08-30 00:00:00 UTC)),
            until_at: None,
        }),
        workspace_id: Some("tend".into()),
        work_id: Some("ENG-7345".into()),
        thread_id: Some("review-cycle".into()),
        kind: Some(RequestKind::Review),
        status: Some(RequestStatus::Completed),
        window: Some("@ENG-7345".into()),
        limit: Some(1),
        offset: 1,
    };

    let page = filters.apply(requests);
    assert_eq!(
        page.iter()
            .map(|request| request.id.as_str())
            .collect::<Vec<_>>(),
        ["second-match"]
    );
}

#[test]
fn message_history_filter_validation_is_actionable() {
    let status = CollaborationListFilters::parse(
        None,
        None,
        None,
        None,
        None,
        Some("unknown"),
        None,
        None,
        0,
    )
    .unwrap_err()
    .to_string();
    assert!(status.contains("queued, claimed, completed"));

    let kind = CollaborationListFilters::parse(
        None,
        None,
        None,
        None,
        Some("unknown"),
        None,
        None,
        None,
        0,
    )
    .unwrap_err()
    .to_string();
    assert!(kind.contains("question, review, task, or notice"));
}

#[test]
fn agent_start_cli_parses_a_structured_pane_launch() {
    let args = Args::try_parse_from([
        "tend",
        "agent",
        "start",
        "--agent",
        "codex",
        "--target",
        "%42",
        "--cwd",
        "/tmp",
        "--prompt",
        "review the changes",
        "--direction",
        "down",
        "--json",
    ])
    .unwrap();
    let Cmd::Agent {
        action: AgentCmd::Start(start),
    } = args.cmd.expect("explicit subcommand")
    else {
        panic!("expected agent start");
    };
    assert_eq!(start.agent, agent_launch::AgentProgram::Codex);
    assert_eq!(start.host, agent_launch::LaunchHost::Auto);
    assert_eq!(start.placement, agent_launch::Placement::Pane);
    assert_eq!(start.target.as_deref(), Some("%42"));
    assert_eq!(start.direction, Some(agent_launch::SplitDirection::Down));
    assert!(start.json);
}

#[test]
fn work_start_cli_pins_ticket_agent_and_role() {
    let args = Args::try_parse_from([
        "tend",
        "work",
        "start",
        "eng-7041",
        "--workspace",
        "tend",
        "--agent",
        "codex",
        "--cwd",
        "/tmp",
        "--role",
        "reviewer",
        "--prompt",
        "review it",
    ])
    .unwrap();
    let Cmd::Work {
        action: WorkCmd::Start(start),
    } = args.cmd.expect("explicit subcommand")
    else {
        panic!("expected work start");
    };
    assert_eq!(start.work, "eng-7041");
    assert_eq!(start.workspace.as_deref(), Some("tend"));
    assert_eq!(start.agent, agent_launch::AgentProgram::Codex);
    assert_eq!(start.role.as_deref(), Some("reviewer"));
}

#[test]
fn work_up_cli_carries_the_pipeline_overrides() {
    let args = Args::try_parse_from([
        "tend",
        "work",
        "up",
        "eng-1234",
        "--pipeline",
        "triad",
        "--body",
        "rebase onto main",
        "--skill",
        "/review-plan",
        "--context",
        "tests pass",
        "--no-ticket",
        "--dry-run",
        "--json",
    ])
    .unwrap();
    let Cmd::Work {
        action: WorkCmd::Up(up),
    } = args.cmd.expect("explicit subcommand")
    else {
        panic!("expected work up");
    };
    assert_eq!(up.work, "eng-1234");
    assert_eq!(up.pipeline.as_deref(), Some("triad"));
    assert_eq!(up.body.as_deref(), Some("rebase onto main"));
    assert_eq!(up.skill.as_deref(), Some("/review-plan"));
    assert_eq!(up.context.as_deref(), Some("tests pass"));
    assert!(up.no_ticket);
    assert!(up.dry_run);
    assert!(up.json);
}

#[test]
fn work_up_still_accepts_prompt_as_a_spelling_of_body() {
    let args =
        Args::try_parse_from(["tend", "work", "up", "eng-1234", "--prompt", "do it"]).unwrap();
    let Cmd::Work {
        action: WorkCmd::Up(up),
    } = args.cmd.expect("explicit subcommand")
    else {
        panic!("expected work up");
    };
    assert_eq!(up.body.as_deref(), Some("do it"));
}

#[test]
fn work_down_is_a_spelling_of_work_close() {
    let args = Args::try_parse_from(["tend", "work", "down", "eng-1234", "-y"]).unwrap();
    let Cmd::Work {
        action: WorkCmd::Down(down),
    } = args.cmd.expect("explicit subcommand")
    else {
        panic!("expected work down");
    };
    assert_eq!(down.work, "eng-1234");
    assert!(down.yes);
}

#[test]
fn window_rename_cli_supports_explicit_automatic_and_buffered_names() {
    let args = Args::try_parse_from([
        "tend",
        "window",
        "rename",
        "ENG-7175 auth refactor",
        "--window",
        "@42",
        "--json",
    ])
    .unwrap();
    let Cmd::Window {
        action: WindowCmd::Rename(rename),
    } = args.cmd.expect("explicit subcommand")
    else {
        panic!("expected window rename");
    };
    assert_eq!(rename.name.as_deref(), Some("ENG-7175 auth refactor"));
    assert!(rename.buffer.is_none());
    assert_eq!(rename.window.as_deref(), Some("@42"));
    assert!(!rename.auto);
    assert!(rename.json);

    let args =
        Args::try_parse_from(["tend", "window", "rename", "--window", "@42", "--auto"]).unwrap();
    assert!(matches!(
        args.cmd.expect("explicit subcommand"),
        Cmd::Window {
            action: WindowCmd::Rename(tmux_work::WindowRenameArgs { auto: true, .. })
        }
    ));
    assert!(Args::try_parse_from([
        "tend", "window", "rename", "name", "--window", "@42", "--auto"
    ])
    .is_err());

    let args = Args::try_parse_from([
        "tend",
        "window",
        "rename",
        "--window",
        "@42",
        "--buffer",
        "tend-window-name-123",
    ])
    .unwrap();
    let Cmd::Window {
        action: WindowCmd::Rename(rename),
    } = args.cmd.expect("explicit subcommand")
    else {
        panic!("expected buffered window rename");
    };
    assert_eq!(rename.buffer.as_deref(), Some("tend-window-name-123"));
    assert!(rename.name.is_none());
}

#[test]
fn workspace_cli_exposes_session_lifecycle() {
    let args = Args::try_parse_from(["tend", "workspace", "show", "tend", "--json"]).unwrap();
    let Cmd::Workspace {
        action: WorkspaceCmd::Show(show),
    } = args.cmd.expect("explicit subcommand")
    else {
        panic!("expected workspace show");
    };
    assert_eq!(show.workspace, "tend");
    assert!(show.json);
}

#[test]
fn agent_control_and_printable_onboarding_parse() {
    let args = Args::try_parse_from([
        "tend",
        "agent",
        "control",
        "--pane",
        "%42",
        "--action",
        "interrupt",
    ])
    .unwrap();
    let Cmd::Agent {
        action: AgentCmd::Control(control),
    } = args.cmd.expect("explicit subcommand")
    else {
        panic!("expected agent control");
    };
    assert_eq!(control.pane.as_deref(), Some("%42"));
    assert!(control.session.is_none());

    let args = Args::try_parse_from([
        "tend",
        "agent",
        "control",
        "--session",
        "pty-7",
        "--action",
        "terminate",
        "--yes",
    ])
    .unwrap();
    let Cmd::Agent {
        action: AgentCmd::Control(control),
    } = args.cmd.expect("explicit subcommand")
    else {
        panic!("expected native agent control");
    };
    assert_eq!(control.session.as_deref(), Some("pty-7"));
    assert!(control.pane.is_none());

    assert!(Args::try_parse_from([
        "tend",
        "agent",
        "control",
        "--pane",
        "%42",
        "--session",
        "pty-7",
        "--action",
        "interrupt",
    ])
    .is_err());

    let args = Args::try_parse_from(["tend", "onboard", "--print", "--no-quiz"]).unwrap();
    let Cmd::Onboard(onboard) = args.cmd.expect("explicit subcommand") else {
        panic!("expected onboard");
    };
    assert!(onboard.print);
    assert!(onboard.no_quiz);
    assert_eq!(onboard.tour, onboarding::Tour::Live);
    assert!(Args::try_parse_from(["tend", "onboard", "--tour", "live"]).is_ok());
    assert!(Args::try_parse_from(["tend", "onboard", "--tour", "simulated"]).is_err());
}

#[test]
fn bare_cli_and_explicit_start_share_the_entry_point() {
    assert!(Args::try_parse_from(["tend"]).unwrap().cmd.is_none());
    assert!(matches!(
        Args::try_parse_from(["tend", "start"]).unwrap().cmd,
        Some(Cmd::Start)
    ));
    let args = Args::try_parse_from(["tend", "onboard", "--quick", "--print"]).unwrap();
    assert!(matches!(
        args.cmd,
        Some(Cmd::Onboard(onboarding::Args {
            quick: true,
            print: true,
            ..
        }))
    ));
    assert!(Args::try_parse_from(["tend", "onboard", "--quick", "--tour", "live"]).is_err());
}

#[test]
fn fleet_attach_accepts_app_fit_mode() {
    assert!(Args::try_parse_from([
            "tend",
            "fleet",
            "attach",
            "--fit",
            "devbox",
            r#"{"window":{"session":{"host":"tmux","socket":"default","session_id":"$1"},"window_id":"@2"},"pane_id":"%3"}"#,
        ])
        .is_ok());
    assert!(Args::try_parse_from(["tend", "fleet-remote-attach", "0123abcd", "--fit",]).is_ok());
}

#[test]
fn watch_theme_cli_aliases_parse() {
    for (raw, expected) in [
        ("oh-my-tend", WatchTheme::OhMyTend),
        ("oh_my_tend", WatchTheme::OhMyTend),
        ("focus", WatchTheme::Focus),
        ("ops", WatchTheme::Ops),
        ("mono", WatchTheme::Mono),
        ("high-contrast", WatchTheme::HighContrast),
        ("high_contrast", WatchTheme::HighContrast),
        ("minimal", WatchTheme::Minimal),
    ] {
        let args = Args::try_parse_from(["tend", "watch", "--theme", raw]).unwrap();
        let Cmd::Watch {
            theme: Some(theme), ..
        } = args.cmd.expect("explicit subcommand")
        else {
            panic!("expected watch theme arg");
        };
        assert_eq!(WatchTheme::from(theme), expected);
    }
}

#[test]
fn table_theme_cli_aliases_parse() {
    for command in ["status", "stats", "timeline", "activity"] {
        let args = Args::try_parse_from(["tend", command, "--theme", "high-contrast"])
            .unwrap_or_else(|err| panic!("{command} should accept --theme: {err}"));
        let theme = match args.cmd.expect("explicit subcommand") {
            Cmd::Status {
                theme: Some(theme), ..
            } => theme,
            Cmd::Stats(args) => args.theme().expect("expected stats theme arg"),
            Cmd::Timeline(args) => args.theme().expect("expected timeline theme arg"),
            Cmd::Activity(args) => args.theme().expect("expected activity theme arg"),
            _ => panic!("expected {command} theme arg"),
        };
        assert_eq!(WatchTheme::from(theme), WatchTheme::HighContrast);
    }
}

#[test]
fn status_json_cli_flag_parses_and_conflicts_with_theme() {
    let args = Args::try_parse_from(["tend", "status", "--json"]).unwrap();
    assert!(matches!(
        args.cmd.expect("explicit subcommand"),
        Cmd::Status {
            json: true,
            theme: None,
            flat: false,
        }
    ));
    assert!(Args::try_parse_from(["tend", "status", "--json", "--theme", "minimal"]).is_err());
    assert!(matches!(
        Args::try_parse_from(["tend", "status", "--flat"])
            .unwrap()
            .cmd
            .expect("explicit subcommand"),
        Cmd::Status { flat: true, .. }
    ));
    assert!(Args::try_parse_from(["tend", "status", "--json", "--flat"]).is_err());
}

#[test]
fn status_json_uses_the_nested_topology_contract() {
    let mut tracked = agent(
        "session-1",
        Some("%7"),
        AgentState::WaitingInput,
        "Approve the deployment?\nMore detail",
    );
    tracked.cwd = Some("/tmp/project".into());
    tracked.last_response = Some("private assistant response".into());
    tracked.last_notification = Some("Approval required".into());
    tracked.context_used_pct = Some(42.5);
    tracked.cost_usd = Some(1.25);

    let generated_at = datetime!(2026-04-24 12:01:00 UTC);
    let value = serde_json::to_value(status_json(
        &[tracked],
        vec![tend::TopologyInput::new(
            tend::HostKind::Tmux,
            vec![pane("%7", "tend")],
        )],
        generated_at,
    ))
    .unwrap();

    assert_eq!(value["schema_version"], 1);
    assert_eq!(value["generated_at"], "2026-04-24T12:01:00Z");
    assert_eq!(value["sessions"][0]["name"], "tend");
    assert_eq!(value["sessions"][0]["windows"][0]["index"], "12");
    let agent = &value["sessions"][0]["windows"][0]["panes"][0]["agent"];
    assert_eq!(agent["kind"], "claude_code");
    assert_eq!(agent["agent_session_id"], "session-1");
    assert!(agent.get("session_id").is_none());
    assert_eq!(agent["state"], "waiting_input");
    assert_eq!(agent["last_prompt"], "Approve the deployment?\nMore detail");
    assert_eq!(agent["context_used_pct"], 42.5);
    assert_eq!(agent["last_response"], "private assistant response");
    assert!(agent.get("rate_limit_5h_pct").is_none());
    assert_eq!(value["sessions"][0]["states"]["waiting_input"], 1);
}

#[test]
fn status_json_exposes_workload_counts_when_present() {
    let generated_at = datetime!(2026-04-24 12:01:00 UTC);
    let mut tracked = agent(
        "session-1",
        Some("%9"),
        AgentState::Working,
        "build the tree",
    );
    tracked.workload = tend::process_tree::WorkloadSummary {
        primary_pid: Some(4321),
        process_count: 4,
        shell_count: 1,
        subagent_count: 2,
        helper_count: 1,
        preview: Vec::new(),
    };
    tracked.subagents = vec![
        tend::state::Subagent {
            kind: "Explore".into(),
            description: Some("map the codebase".into()),
            started_at: generated_at,
        },
        tend::state::Subagent {
            kind: "general-purpose".into(),
            description: None,
            started_at: generated_at,
        },
    ];

    let value = serde_json::to_value(status_json(
        &[tracked],
        vec![tend::TopologyInput::new(
            tend::HostKind::Tmux,
            vec![pane("%9", "tend")],
        )],
        generated_at,
    ))
    .unwrap();

    let agent = &value["sessions"][0]["windows"][0]["panes"][0]["agent"];
    let wl = &agent["workload"];
    assert_eq!(wl["subagent_count"], 2);
    assert_eq!(wl["shell_count"], 1);
    assert_eq!(wl["process_count"], 4);
    assert_eq!(wl["helper_count"], 1);
    assert!(wl.get("preview").is_none());

    // Named, hook-tracked Task subagents ride in a sibling array.
    let subs = &agent["subagents"];
    assert_eq!(subs[0]["kind"], "Explore");
    assert_eq!(subs[0]["description"], "map the codebase");
    assert_eq!(subs[0]["started_at"], "2026-04-24T12:01:00Z");
    assert_eq!(subs[1]["kind"], "general-purpose");
    assert!(subs[1].get("description").is_none());
}

const ALL_STATES: [AgentState; 7] = [
    AgentState::Working,
    AgentState::WaitingInput,
    AgentState::WaitingChoice,
    AgentState::Error,
    AgentState::Idle,
    AgentState::Starting,
    AgentState::Stopped,
];

#[test]
fn status_line_icons_are_single_cell() {
    for state in ALL_STATES {
        assert_eq!(UnicodeWidthStr::width(state_icon(state)), 1);
    }
}

#[test]
fn attention_segment_is_empty_when_all_clear() {
    // Nothing blocked → empty string so the tmux segment disappears.
    assert_eq!(attention_segment(0), "");
}

#[test]
fn attention_segment_renders_count_with_tmux_markup() {
    let seg = attention_segment(2);
    assert_eq!(seg, "#[fg=red]⚠ 2 need you#[default]");
    // tmux styling, never raw ANSI (no ESC).
    assert!(!seg.contains('\u{1b}'));
    // Wraps the visible text in tmux color markup.
    assert!(seg.starts_with("#[fg=red]"));
    assert!(seg.ends_with("#[default]"));
    assert!(seg.contains("2 need you"));

    // Count is faithfully interpolated for other N, with subject-verb
    // agreement (singular "needs", plural "need").
    assert!(attention_segment(1).contains("1 needs you"));
    assert!(attention_segment(7).contains("7 need you"));
}

#[test]
fn icon_sets_are_single_cell_and_distinct() {
    // Both glyph sets must stay one cell wide and unambiguous so the
    // [ui] icons toggle never breaks column alignment or readability.
    for build in [state_icon_unicode, state_icon_ascii] {
        let mut seen = std::collections::HashSet::new();
        for state in ALL_STATES {
            let glyph = build(state);
            assert_eq!(
                UnicodeWidthStr::width(glyph),
                1,
                "{glyph:?} not single-cell"
            );
            assert!(seen.insert(glyph), "duplicate glyph {glyph:?}");
        }
    }
    // The ascii set must be pure ASCII to survive font-less terminals.
    for state in ALL_STATES {
        assert!(state_icon_ascii(state).is_ascii());
    }
}

#[test]
fn relative_time_units() {
    let now = datetime!(2026-04-24 12:00:00 UTC);
    assert_eq!(relative_time(now, now), "0s ago");
    assert_eq!(
        relative_time(now, datetime!(2026-04-24 11:59:30 UTC)),
        "30s ago"
    );
    assert_eq!(
        relative_time(now, datetime!(2026-04-24 11:55:00 UTC)),
        "5m ago"
    );
    assert_eq!(
        relative_time(now, datetime!(2026-04-24 10:00:00 UTC)),
        "2h ago"
    );
    assert_eq!(
        relative_time(now, datetime!(2026-04-22 12:00:00 UTC)),
        "2d ago"
    );
}

#[test]
fn relative_time_future_clamped() {
    let now = datetime!(2026-04-24 12:00:00 UTC);
    // Clock skew or reordering — don't emit negative strings.
    assert_eq!(
        relative_time(now, datetime!(2026-04-24 12:00:30 UTC)),
        "0s ago"
    );
}

#[test]
fn truncate_cell_clips_to_ascii_ellipsis() {
    assert_eq!(truncate_cell("short", 10), "short");
    assert_eq!(truncate_cell("0123456789abc", 8), "01234...");
    assert_eq!(truncate_cell("한글테스트입니다", 6), "한...");
}

/// `tend status` nests agents under their window and session, folds a
/// grouped `~view~` session onto its base, and lists paneless agents last.
#[test]
fn status_tree_groups_agents_by_session_and_window() {
    let pane = |id: &str, session: &str, session_id: &str, window: &str, index: &str| {
        serde_json::from_value::<tend::tmux::PaneInfo>(serde_json::json!({
            "pane_id": id, "session": session, "session_id": session_id,
            "session_group": "webapp", "window_id": format!("@{window}"),
            "window_name": format!("w{window}"), "window_index": window,
            "pane_index": index, "tty": "", "current_command": "aas", "title": "",
            "pane_pid": 0, "socket": "default"
        }))
        .unwrap()
    };
    let panes = vec![
        pane("%1", "webapp", "$1", "0", "0"),
        pane("%1", "webapp~view~9", "$9", "0", "0"),
        pane("%2", "webapp", "$1", "1", "0"),
        pane("%3", "webapp", "$1", "1", "1"),
    ];
    let mut agents = vec![
        agent("a", Some("%1"), AgentState::Working, "first task"),
        agent("b", Some("%2"), AgentState::Idle, "second"),
        agent("c", Some("%3"), AgentState::WaitingInput, "third"),
        agent("d", None, AgentState::Idle, "background"),
    ];
    for a in &mut agents {
        a.tmux_socket = a.pane.as_ref().map(|_| "default".to_owned());
    }
    let now = OffsetDateTime::now_utc();
    let topology = tend::TopologySnapshot::build(
        now,
        vec![tend::TopologyInput::new(tend::HostKind::Tmux, panes)],
        agents,
    );
    let rendered = render_status_tree(&topology, now, CliTheme::plain(), 120);
    let lines: Vec<&str> = rendered
        .lines()
        .map(|line| line.strip_prefix("│ ").unwrap_or(line).trim_end())
        .collect();
    let at = |needle: &str| {
        lines
            .iter()
            .position(|line| line.contains(needle))
            .unwrap_or_else(|| panic!("{needle:?} missing:\n{rendered}"))
    };
    assert!(!rendered.contains("~view~"), "{rendered}");
    assert!(lines[at("webapp")].contains("1 working · 1 waiting · 1 idle"));
    // A window with one agent carries it on its own line.
    assert!(at("webapp") < at("├─ 0:w0 · claude"));
    assert!(lines[at("├─ 0:w0 · claude")].contains("first task"));
    assert!(at("├─ 0:w0 · claude") < at("└─ 1:w1"));
    assert!(at("└─ 1:w1") < at("   ├─ 0 claude"));
    assert!(at("   ├─ 0 claude") < at("   └─ 1 claude"));
    assert!(at("└─ 1:w1") < at("(no pane)"));
    assert!(lines[at("(no pane)") + 1].contains("background"));
}

/// One agent in a session is one line; one window drops its row.
#[test]
fn status_tree_collapses_single_window_and_single_agent_sessions() {
    let pane = |id: &str, session: &str, index: &str| {
        serde_json::from_value::<tend::tmux::PaneInfo>(serde_json::json!({
            "pane_id": id, "session": session, "session_id": format!("${session}"),
            "window_id": format!("@{session}"), "window_name": "zsh",
            "window_index": "0", "pane_index": index, "tty": "",
            "current_command": "aas", "title": "", "pane_pid": 0, "socket": "default"
        }))
        .unwrap()
    };
    let panes = vec![
        pane("%1", "solo", "0"),
        pane("%2", "pair", "0"),
        pane("%3", "pair", "1"),
    ];
    let mut agents = vec![
        agent("a", Some("%1"), AgentState::Idle, "solo prompt"),
        agent("b", Some("%2"), AgentState::Working, "left"),
        agent("c", Some("%3"), AgentState::Idle, "right"),
    ];
    for a in &mut agents {
        a.tmux_socket = Some("default".to_owned());
    }
    let now = OffsetDateTime::now_utc();
    let topology = tend::TopologySnapshot::build(
        now,
        vec![tend::TopologyInput::new(tend::HostKind::Tmux, panes)],
        agents,
    );
    let rendered = render_status_tree(&topology, now, CliTheme::plain(), 120);
    let lines: Vec<&str> = rendered.lines().collect();
    let solo: Vec<_> = lines.iter().filter(|l| l.contains("solo")).collect();
    assert_eq!(solo.len(), 1, "{rendered}");
    assert!(solo[0].contains("solo · claude") && solo[0].contains("solo prompt"));
    assert!(
        !rendered.contains("0:zsh"),
        "single windows drop their row:\n{rendered}"
    );
    assert!(rendered.contains("├─ 0 claude") && rendered.contains("└─ 1 claude"));
}

#[test]
fn pane_inventory_lists_grouped_panes_once_with_their_view_count() {
    let pane = |session: &str, session_id: &str| {
        serde_json::from_value::<tend::tmux::PaneInfo>(serde_json::json!({
            "pane_id": "%2", "session": session, "session_id": session_id,
            "window_index": "2", "pane_index": "0", "tty": "/dev/pts/9",
            "current_command": "aas", "title": "t", "pane_pid": 0, "socket": "default"
        }))
        .unwrap()
    };
    let lines = pane_inventory_lines(&[
        pane("webapp~view~1", "$8"),
        pane("webapp", "$0"),
        pane("webapp~view~2", "$9"),
    ]);
    assert_eq!(lines.len(), 1, "{lines:?}");
    assert!(lines[0].contains("webapp:2.0  +2 views"), "{lines:?}");
}

#[test]
fn pane_display_shows_task_name_when_paneless() {
    // A paneless background task shows its registered name in NAME...
    let mut task = agent("game", None, AgentState::Working, "");
    task.kind = AgentKind::Task;
    assert_eq!(pane_display(&task, &[]), "game");
    // ...while other paneless agents (e.g. SDK sessions) still show "-".
    let sdk = agent("sdk-1", None, AgentState::Idle, "");
    assert_eq!(pane_display(&sdk, &[]), "-");
}

/// `tend status` from a plain terminal, where the live listing lacks the
/// agents' host: the recorded session still names them, and a pane id
/// on another server is never taken for this one.
#[test]
fn pane_display_matches_the_server_and_falls_back_to_the_session() {
    let mut agent = agent("cx", Some("rmux:%2"), AgentState::Idle, "");
    agent.tmux_socket = Some("/tmp/rmux-1/default".into());
    agent.tmux_session = Some("webapp~view~7".into());
    assert_eq!(pane_display(&agent, &[]), "webapp:%2");

    let pane = |socket: &str, session: &str| -> tend::tmux::PaneInfo {
        serde_json::from_value(serde_json::json!({
            "pane_id": "rmux:%2", "session": session, "window_index": "2",
            "pane_index": "0", "tty": "", "current_command": "aas", "title": "",
            "pane_pid": 0, "socket": socket
        }))
        .unwrap()
    };
    let panes = [
        pane("/tmp/rmux-9/other", "elsewhere"),
        pane("/tmp/rmux-1/default", "webapp"),
    ];
    assert_eq!(pane_display(&agent, &panes), "webapp:2.0");
    agent.tmux_session = None;
    assert_eq!(pane_display(&agent, &panes[..1]), "rmux:%2");
}

#[test]
fn status_table_compacts_without_wrapping_at_88_cols() {
    let agents = vec![
        agent(
            "a",
            Some("%1"),
            AgentState::WaitingChoice,
            "please choose one of the available deployment options",
        ),
        agent(
            "b",
            Some("%2"),
            AgentState::Working,
            "this is a very long prompt that should be truncated in compact status output",
        ),
    ];
    let panes = vec![
        pane("%1", "webapp-knowledge-long-session-name"),
        pane("%2", "tend"),
    ];

    let rendered = render_status_table(
        &agents,
        &panes,
        datetime!(2026-04-24 12:00:00 UTC),
        CliTheme::plain(),
        88,
    );

    assert!(rendered.contains("choice"));
    assert!(!rendered.contains("MODEL"));
    assert!(rendered.contains("webapp-knowledg..."));
    for line in rendered.lines() {
        assert!(
            UnicodeWidthStr::width(line) <= 88,
            "line exceeded compact status width: {line:?}"
        );
    }
}

#[test]
fn status_table_minimal_layout_for_very_narrow_width() {
    let agents = vec![agent(
        "a",
        Some("%1"),
        AgentState::WaitingInput,
        "approve permission",
    )];
    let panes = vec![pane("%1", "webapp-set")];

    let rendered = render_status_table(
        &agents,
        &panes,
        datetime!(2026-04-24 12:00:00 UTC),
        CliTheme::plain(),
        60,
    );

    assert!(rendered.contains("ST"));
    assert!(rendered.contains("ACT"));
    assert!(rendered.contains("NAME"));
    assert!(!rendered.contains("KIND"));
    assert!(!rendered.contains("MODEL"));
    for line in rendered.lines() {
        assert!(
            UnicodeWidthStr::width(line) <= 60,
            "line exceeded minimal status width: {line:?}"
        );
    }
}
#[test]
fn retirement_defaults_to_preview_and_requires_exact_id_reason_and_evidence() {
    let base = [
        "tend",
        "msg",
        "retire",
        "req-exact",
        "--reason",
        "withdrawn",
        "--evidence",
        "update:2",
    ];
    let preview = Args::try_parse_from(base).unwrap();
    assert!(matches!(
        preview.cmd,
        Some(Cmd::Msg {
            action: MsgCmd::Retire { execute: false, .. }
        })
    ));
    let mut execute = base.to_vec();
    execute.push("--execute");
    assert!(matches!(
        Args::try_parse_from(execute).unwrap().cmd,
        Some(Cmd::Msg {
            action: MsgCmd::Retire { execute: true, .. }
        })
    ));
    assert!(Args::try_parse_from([
        "tend",
        "msg",
        "retire",
        "req-exact",
        "--reason",
        "withdrawn"
    ])
    .is_err());
    assert!(Args::try_parse_from([
        "tend",
        "msg",
        "retire",
        "--reason",
        "withdrawn",
        "--evidence",
        "update:2"
    ])
    .is_err());
}

#[test]
fn inbox_json_is_compact_by_default_and_full_records_are_explicit() {
    for (args, expected_full) in [
        (vec!["tend", "msg", "inbox", "--json"], false),
        (vec!["tend", "msg", "inbox", "--json", "--full"], true),
    ] {
        let parsed = Args::try_parse_from(args).unwrap();
        assert!(
            matches!(parsed.cmd,Some(Cmd::Msg{action:MsgCmd::Inbox{json:true,full}}) if full==expected_full)
        );
    }
    assert!(Args::try_parse_from(["tend", "msg", "inbox", "--full"]).is_err());
    let parsed = Args::try_parse_from(["tend", "msg", "accept", "req-exact", "--json"]).unwrap();
    assert!(
        matches!(parsed.cmd,Some(Cmd::Msg{action:MsgCmd::Accept{request_id,json:true}}) if request_id=="req-exact")
    );
}
#[test]
fn mutation_json_has_an_explicit_legacy_full_escape() {
    for words in [
        vec!["send", "peer", "body"],
        vec!["reply", "req-test", "body"],
        vec!["cancel", "req-test"],
        vec!["attention", "body"],
    ] {
        let mut args = vec!["tend", "msg"];
        args.extend(words);
        args.extend(["--json", "--full"]);
        assert!(Args::try_parse_from(args).is_ok());
    }
}
