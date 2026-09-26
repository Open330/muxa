//! Compare the former full-list read paths with scoped runtime queries.
//! Run: `cargo bench -p muxa --bench runtime_reads`
//! Synthetic workloads; results describe these reads, not end-to-end latency.
#![allow(clippy::cast_precision_loss)]

use std::hint::black_box;
use std::time::Instant;

use muxa::event::{AgentEvent, AgentId, AgentKind};
use muxa::fleet::{FleetHostSnapshot, FleetStore};
use muxa::pipeline::DesiredAgent;
use muxa::pipeline_run::{PipelineRun, PipelineRunRegistration, PipelineRunStore};
use muxa::work::WorkIdentity;
use time::OffsetDateTime;

const ITERS: u32 = 200;

async fn fleet_reads(count: usize) {
    let agents = muxa::Store::shared();
    let id = AgentId {
        tmux_socket: None,
        kind: AgentKind::Codex,
        session_id: "bench".into(),
        surface: None,
        pane: Some("%1".into()),
        cwd: None,
    };
    agents
        .apply(&AgentEvent::Started {
            id,
            at: OffsetDateTime::now_utc(),
        })
        .await;
    let mut agent = agents.snapshot().await.pop().unwrap();
    agent.last_prompt = Some("p".repeat(4096));
    agent.last_response = Some("r".repeat(4096));
    let store = FleetStore::new();
    for index in 0..count {
        let host: FleetHostSnapshot = serde_json::from_value(serde_json::json!({
            "alias": format!("host-{index}"), "ssh_target": "bench", "labels": {},
            "annotations": {}, "mode": "observe", "state": "online",
            "remote": { "revision": 1, "observed_at": "2026-09-26T00:00:00Z",
                "agents": vec![agent.clone(); 10], "panes": [], "sessions": [], "backends": [] }
        }))
        .unwrap();
        store.upsert_host(host).await;
    }
    let alias = format!("host-{}", count - 1);
    for _ in 0..10 {
        black_box(store.snapshot().await);
        black_box(store.host_snapshot(&alias).await);
    }
    let start = Instant::now();
    for _ in 0..ITERS {
        black_box(
            store
                .snapshot()
                .await
                .hosts
                .into_iter()
                .find(|host| host.alias == alias),
        );
    }
    let full = start.elapsed();
    let start = Instant::now();
    for _ in 0..ITERS {
        black_box(store.host_snapshot(&alias).await);
    }
    let scoped = start.elapsed();
    println!(
        "fleet hosts={count} agents/host=10 payload/agent=8KiB full_us={:.2} scoped_us={:.2}",
        full.as_secs_f64() * 1e6 / f64::from(ITERS),
        scoped.as_secs_f64() * 1e6 / f64::from(ITERS)
    );
}

async fn pipeline_reads(count: usize) {
    let store = PipelineRunStore::in_memory();
    for index in 0..count {
        let run = store
            .register(PipelineRunRegistration {
                identity: WorkIdentity::new("bench", format!("run-{index}")),
                pipeline: "solo".into(),
                desired: vec![DesiredAgent {
                    alias: "impl".into(),
                    program: "codex".into(),
                    role: None,
                    task: None,
                    prompt: Some("p".repeat(32768)),
                    options: Vec::new(),
                    direction: None,
                    after: Vec::new(),
                }],
                cwd: "/tmp".into(),
                window_id: None,
                observed: Vec::new(),
                invalidate: Vec::new(),
            })
            .await
            .unwrap();
        // No early exit: every Run has an active claim during the measurement.
        store
            .claim_ready(&run.identity, run.generation)
            .await
            .unwrap();
    }
    assert!(!store.has_ready_alias().await);
    for _ in 0..10 {
        black_box(store.list().await);
        black_box(store.has_ready_alias().await);
    }
    let start = Instant::now();
    for _ in 0..ITERS {
        black_box(store.list().await.iter().any(PipelineRun::has_ready_alias));
    }
    let full = start.elapsed();
    let start = Instant::now();
    for _ in 0..ITERS {
        black_box(store.has_ready_alias().await);
    }
    let scoped = start.elapsed();
    println!(
        "pipeline runs={count} prompt/run=32KiB full_us={:.2} readiness_us={:.2}",
        full.as_secs_f64() * 1e6 / f64::from(ITERS),
        scoped.as_secs_f64() * 1e6 / f64::from(ITERS)
    );
}

fn main() {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(async {
            for count in [1, 10, 100] {
                fleet_reads(count).await;
            }
            for count in [1, 100, 1000] {
                pipeline_reads(count).await;
            }
        });
}
