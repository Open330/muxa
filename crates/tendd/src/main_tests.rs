use super::*;
use tend::config::DiscoveryConfig;

#[test]
fn only_the_default_daemon_owns_the_data_directory() {
    let default_socket = paths::default_socket();
    let config = std::path::Path::new("/home/u/.config/tend/config.toml");
    let other = std::path::Path::new("/tmp/t/tend-e2e.toml");
    // The service and `tend daemon start`: canonical socket, default config.
    assert!(owns_default_data_dir(
        &default_socket,
        None,
        None,
        Some(config)
    ));
    assert!(owns_default_data_dir(
        &default_socket,
        None,
        Some(config),
        Some(config)
    ));
    // The socket named in config is canonical too.
    let named = std::path::Path::new("/run/tend/custom.sock");
    assert!(owns_default_data_dir(
        named,
        Some(named),
        None,
        Some(config)
    ));
    // e2e and the onboarding sandbox: their own socket, their own config.
    let tmp = std::path::Path::new("/tmp/t/tend-e2e.sock");
    assert!(!owns_default_data_dir(tmp, None, Some(other), Some(config)));
    assert!(!owns_default_data_dir(tmp, None, None, Some(config)));
    assert!(!owns_default_data_dir(
        &default_socket,
        None,
        Some(other),
        Some(config)
    ));
}

#[test]
fn dashboard_auth_parser_accepts_public_read_aliases() {
    assert_eq!(
        parse_dashboard_auth("public_read").unwrap(),
        DashboardAuthMode::PublicRead
    );
    assert_eq!(
        parse_dashboard_auth("read-only").unwrap(),
        DashboardAuthMode::PublicRead
    );
}

#[test]
fn heals_canonical_default_or_configured_socket() {
    let default = Path::new("/run/user/1000/tend.sock");
    let configured = Path::new("/custom/primary.sock");
    // Default socket → the primary daemon, may heal.
    assert!(should_heal_tmux_socket_env(default, default, None));
    // Socket named in config → also the primary, may heal even when
    // it differs from the XDG/default path.
    assert!(should_heal_tmux_socket_env(
        configured,
        default,
        Some(configured)
    ));
}

#[test]
fn skips_ephemeral_override_socket() {
    // A dashboard demo / e2e daemon on a throwaway socket matching
    // neither the default nor the configured one must NOT clobber the
    // tmux global env the primary daemon owns — that poisoning is
    // exactly what strands later panes on a dead `tend-dash.sock`.
    let default = Path::new("/run/user/1000/tend.sock");
    let configured = Path::new("/custom/primary.sock");
    let ephemeral = Path::new("/tmp/.tmpXYZ/tend-dash.sock");
    assert!(!should_heal_tmux_socket_env(ephemeral, default, None));
    assert!(!should_heal_tmux_socket_env(
        ephemeral,
        default,
        Some(configured)
    ));
}

#[tokio::test]
async fn startup_discovery_runs_when_enabled() {
    let cfg = Config {
        discovery: DiscoveryConfig {
            enabled: true,
            ..DiscoveryConfig::default()
        },
        ..Config::default()
    };
    let (shutdown_tx, _) = broadcast::channel::<()>(1);
    let spawned = spawn_startup_discovery(
        &cfg,
        PathBuf::from("/tmp/never-bound.sock"),
        tend::active_backends(),
        &shutdown_tx,
    );
    assert!(spawned, "discovery should spawn when enabled");
}

#[tokio::test]
async fn startup_discovery_skipped_when_disabled() {
    let cfg = Config {
        discovery: DiscoveryConfig {
            enabled: false,
            ..DiscoveryConfig::default()
        },
        ..Config::default()
    };
    let (shutdown_tx, _) = broadcast::channel::<()>(1);
    let spawned = spawn_startup_discovery(
        &cfg,
        PathBuf::from("/tmp/never-bound.sock"),
        tend::active_backends(),
        &shutdown_tx,
    );
    assert!(!spawned, "discovery must not spawn when disabled");
}

#[tokio::test]
async fn a_corrupt_mailbox_never_silently_falls_back_to_memory() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("corrupt.sqlite");
    std::fs::write(&path, "not a database").unwrap();
    let mut config = Config::default();
    config.collaboration.enabled = true;
    config.collaboration.path = Some(path);
    assert!(build_collaboration(&config)
        .await
        .unwrap()
        .initialization_error()
        .is_some());
}

#[test]
fn the_daemon_runtime_does_not_wait_out_a_stuck_blocking_task() {
    let started = std::time::Instant::now();
    let result = run_bounded(
        async {
            tokio::task::spawn_blocking(|| {
                std::thread::sleep(std::time::Duration::from_secs(30));
            });
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            Ok(())
        },
        std::time::Duration::from_millis(200),
    );
    assert!(result.is_ok());
    assert!(
        started.elapsed() < std::time::Duration::from_secs(5),
        "teardown took {:?}",
        started.elapsed()
    );
}

#[tokio::test]
async fn only_the_default_daemon_finishes_interrupted_completions() {
    use tend::pipeline_run::{PendingCompletion, PipelineAliasStatus, PipelineRunRegistration};
    let identity = tend::work::WorkIdentity::new("ws", "WORK-1");
    let runs = PipelineRunStore::in_memory();
    let run = runs
        .register(PipelineRunRegistration {
            identity: identity.clone(),
            pipeline: "solo".to_string(),
            desired: vec![tend::pipeline::DesiredAgent {
                alias: "impl".to_string(),
                program: "codex".to_string(),
                role: None,
                task: None,
                prompt: Some("do it".to_string()),
                options: Vec::new(),
                direction: None,
                after: Vec::new(),
                purpose: tend::collaboration::LaunchPurpose::Primary,
            }],
            cwd: PathBuf::from("/tmp/work"),
            window_id: None,
            observed: Vec::new(),
            invalidate: Vec::new(),
        })
        .await
        .unwrap();
    runs.claim_ready(&identity, run.generation).await.unwrap();
    let record = tend::review::NewReview {
        target: tend::review::ReviewTarget::Work {
            workspace: "ws".to_string(),
            work: "WORK-1".to_string(),
            alias: Some("impl".to_string()),
            pipeline_generation: Some(run.generation),
        },
        summary: "done".to_string(),
        anchor: None,
        idempotency_key: Some("work-done:startup-test".to_string()),
        requested_by: tend::review::ReviewActor {
            kind: tend::review::ActorKind::Agent,
            label: None,
        },
        decision: None,
    };
    runs.begin_completion(
        &identity,
        "impl",
        PendingCompletion {
            generation: run.generation,
            record,
            verification: None,
        },
    )
    .await
    .unwrap();
    let reviews = tend::review::ReviewStore::in_memory();
    let alias = |runs: Vec<tend::pipeline_run::PipelineRun>| {
        let state = runs[0].aliases["impl"].clone();
        (state.status, state.completing.is_some())
    };

    // An override daemon leaves it alone: the stores may be the default one's.
    assert!(!recover_completions(&runs, &reviews, false).await);
    assert_eq!(
        alias(runs.list().await),
        (PipelineAliasStatus::Running, true)
    );

    assert!(recover_completions(&runs, &reviews, true).await);
    assert_eq!(alias(runs.list().await), (PipelineAliasStatus::Done, false));
}
