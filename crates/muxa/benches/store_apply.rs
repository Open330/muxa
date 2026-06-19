//! Microbenchmark for `Store::apply` on the broadcast hot path.
//!
//! Hypothesis under test: `Transition` carries a full `Agent` (a struct
//! with several `String` fields, including up to ~4 KB of `last_prompt`
//! and `last_response`), and the in-process `tokio::sync::broadcast`
//! channel clones the payload once per subscriber on every `recv()`.
//! With N broadcast subscribers, the per-`apply` cost should scale
//! roughly as N × `sizeof(Agent)` bytes of allocation + copy.
//!
//! Strategy:
//!   - Pre-populate the store with one realistic `Agent` carrying a 4 KB
//!     `last_prompt` and a 4 KB `last_response`.
//!   - Spawn N "drainer" tasks, each holding a `broadcast::Receiver` and
//!     `recv`-looping into `std::hint::black_box` so the compiler can't
//!     drop the clone.
//!   - In the main task, call `Store::apply(PromptSubmitted)` in a
//!     tight loop and time wall-clock with `Instant`.
//!
//! Run with: `cargo bench -p muxa --bench store_apply`
//! (release mode is implicit for `cargo bench`).
//!
//! Output is a small table: N subscribers, iterations, total ms,
//! ns/iter, transitions/sec.

// Bench code is allowed to be sloppy about timing-arithmetic precision —
// we're printing human-readable µs/sec figures, not feeding the values
// into anything else. Allowing these at the file level is simpler than
// peppering the print loop with attribute noise.
#![allow(clippy::cast_precision_loss)]

use std::hint::black_box;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use muxa::event::{AgentEvent, AgentId, AgentKind};
use muxa::state::{Agent, Store};
use time::OffsetDateTime;
use tokio::runtime::Builder;
use tokio::sync::broadcast;

/// Build a realistic Agent with ~4 KB `last_prompt` and ~4 KB
/// `last_response`, plus the other String fields populated. This is the
/// worst-case payload the broadcast machinery has to clone on every
/// state change.
fn make_realistic_agent() -> Agent {
    let now = OffsetDateTime::now_utc();
    let prompt = "x".repeat(4096);
    let response = "y".repeat(4096);
    Agent {
        kind: AgentKind::ClaudeCode,
        session_id: "sess-bench-0001".into(),
        surface: None,
        pane: Some("%42".into()),
        pid: None,
        workload: muxa::WorkloadSummary::default(),
        cwd: Some("/home/user/projects/some-large-codebase".into()),
        state: muxa::AgentState::Idle,
        last_prompt: Some(prompt),
        last_response: Some(response),
        last_notification: Some("permission needed for tool: Bash(rm -rf /tmp/cache)".into()),
        model: Some("claude-opus-4-7".into()),
        context_used_pct: Some(48.5),
        cost_usd: Some(2.34),
        rate_limit_5h_pct: Some(72.0),
        rate_limit_5h_resets_at: Some(now + time::Duration::hours(1)),
        rate_limit_7d_pct: Some(34.0),
        rate_limit_7d_resets_at: Some(now + time::Duration::days(3)),
        rate_limited_until: None,
        rate_limit_scope: None,
        rate_limit_source: None,
        started_at: now,
        last_activity_at: now,
        state_entered_at: now,
    }
}

/// Spawn `n` drainer tasks. Each owns a `broadcast::Receiver<Transition>`
/// and pulls Transitions in a loop, black-boxing them so the optimiser
/// can't elide the clone the broadcast made. Each drainer keeps running
/// until it has observed `expected_per_sub` Transitions, then exits.
///
/// Returns the join handles so the caller can wait for full end-to-end
/// drain — this is the metric that captures both producer cost AND
/// per-subscriber clone cost, which is what the Arc refactor actually
/// changes.
fn spawn_drainers(
    store: &Arc<Store>,
    n: usize,
    expected_per_sub: u64,
) -> (Arc<AtomicU64>, Vec<tokio::task::JoinHandle<()>>) {
    let counter = Arc::new(AtomicU64::new(0));
    let mut handles = Vec::with_capacity(n);
    for _ in 0..n {
        let mut rx = store.subscribe();
        let c = counter.clone();
        handles.push(tokio::spawn(async move {
            let mut seen: u64 = 0;
            while seen < expected_per_sub {
                match rx.recv().await {
                    Ok(t) => {
                        // black_box the whole Transition so the clone the
                        // broadcast machinery did is observable to the
                        // compiler. Without this, LLVM might decide the
                        // Transition is dead and skip the work entirely.
                        black_box(&t);
                        c.fetch_add(1, Ordering::Relaxed);
                        seen += 1;
                    }
                    Err(broadcast::error::RecvError::Lagged(missed)) => {
                        // Count lagged messages toward the per-sub quota
                        // so a slow subscriber doesn't deadlock the bench
                        // — we already observed (in the producer) that
                        // those Transitions were sent.
                        seen = seen.saturating_add(missed);
                    }
                    Err(broadcast::error::RecvError::Closed) => return,
                }
            }
        }));
    }
    (counter, handles)
}

/// Run a single benchmark configuration: N subscribers, ITERS
/// `PromptSubmitted` applies. Returns the wall-clock duration spent
/// inside the apply loop.
async fn bench_one(n_subscribers: usize, iters: u64) -> (Duration, Duration, u64) {
    let store = Arc::new(Store::default());

    // Seed an agent so PromptSubmitted lands a transition (Idle → Working
    // emits one Transition, every iteration). We use `apply` itself with
    // a Started event to seed identity fields and reach Idle, then warm
    // the Working/Idle ping-pong.
    let id = AgentId {
        kind: AgentKind::ClaudeCode,
        session_id: "sess-bench-0001".into(),
        surface: None,
        pane: Some("%42".into()),
        cwd: Some("/home/user/projects/some-large-codebase".into()),
    };
    let now = OffsetDateTime::now_utc();
    store
        .apply(&AgentEvent::Started {
            id: id.clone(),
            at: now,
        })
        .await;

    // Inflate the agent's last_prompt/last_response/last_notification so
    // the broadcast clone really has to copy 4 KB-ish of String data.
    // Easiest path: hydrate over the top with a realistic Agent. This
    // bypasses apply, which is intentional — we want the fields populated
    // before we start measuring, not as part of the measurement.
    store.hydrate(vec![make_realistic_agent()]).await;

    // Subscribers must be live before we start applying so they actually
    // hold the cursor during the bench. broadcast drops messages emitted
    // before any subscriber existed.
    //
    // Each drainer needs to see 2 * iters Transitions (PromptSubmitted +
    // TurnStopped per outer iter). We cap the drainer's loop at that
    // count so the bench can join them — `JoinHandle::await` on the full
    // set is the end-to-end "all subscribers caught up" boundary.
    let (counter, handles) = spawn_drainers(&store, n_subscribers, 2 * iters);

    // Give the drainers a tick to install their cursors. tokio::yield_now
    // is enough on a multi-threaded runtime; a 1ms sleep is belt-and-braces.
    tokio::time::sleep(Duration::from_millis(1)).await;

    // Build one PromptSubmitted event up-front and reuse it. Cloning the
    // event itself is part of the realistic per-apply work but we want
    // to keep the iteration loop body as pure as possible — the prompt
    // String is 4 KB and we don't want String::clone of the prompt to
    // dominate the timing on the apply side.
    //
    // (apply mutates agent.last_prompt = prompt.clone() inside; that's
    // unavoidable and IS part of what we're measuring.)
    let ev = AgentEvent::PromptSubmitted {
        id: id.clone(),
        prompt: "z".repeat(4096),
        at: now,
    };
    // After a PromptSubmitted, state is Working. To get a transition every
    // iteration we alternate with TurnStopped (which flips back to Idle).
    let stop = AgentEvent::TurnStopped {
        id: id.clone(),
        response: Some("r".repeat(4096)),
        at: now,
    };

    let start = Instant::now();
    for _ in 0..iters {
        store.apply(&ev).await; // Idle -> Working, emits Transition
        store.apply(&stop).await; // Working -> Idle, emits Transition
    }
    let producer_elapsed = start.elapsed();

    // Wait for all subscribers to fully drain the broadcast. The
    // end-to-end span captures both producer cost and per-subscriber
    // clone cost — exactly the system-wide metric the Arc refactor is
    // meant to improve.
    for h in handles {
        let _ = h.await;
    }
    let total_elapsed = start.elapsed();

    // Each iteration emits 2 transitions, so the drainers should see
    // roughly 2 * iters * n_subscribers events. We don't assert this
    // (Lagged is allowed) but we do report it for sanity.
    let observed = counter.load(Ordering::Relaxed);
    (producer_elapsed, total_elapsed, observed)
}

fn main() {
    // Multi-threaded runtime — the drainer tasks need to be polled in
    // parallel with the producer for the benchmark to actually exercise
    // the broadcast fanout. A current-thread runtime would serialise
    // the producer behind whichever drainer happens to wake first.
    let rt = Builder::new_multi_thread()
        .worker_threads(num_cpus())
        .enable_all()
        .build()
        .expect("build tokio runtime");

    println!("# muxa Store::apply broadcast benchmark");
    println!("# Agent: 4 KB last_prompt + 4 KB last_response + small fields");
    println!("# Each iter = 2 applies (PromptSubmitted + TurnStopped) = 2 Transitions");
    println!("# 'producer' = Store::apply loop only");
    println!("# 'e2e'      = producer + wait for every subscriber to fully drain");
    println!();
    println!(
        "{:>5} {:>7} {:>11} {:>11} {:>11} {:>11} {:>13} {:>13}",
        "subs", "iters", "prod ms", "us/apply", "e2e ms", "us/apply", "rx total", "expected"
    );

    // Warmup pass to JIT-warm the allocator / runtime.
    let _ = rt.block_on(bench_one(2, 1_000));

    for &n in &[0_usize, 1, 2, 4, 8, 16] {
        // 2 applies per iter so 5_000 iters = 10_000 applies.
        // At a few µs per apply, this is tens of ms of wall time per row,
        // well above the timer's noise floor on Linux.
        let iters: u64 = 5_000;
        let (prod_dur, e2e_dur, observed) = rt.block_on(bench_one(n, iters));
        let total_applies = (iters * 2) as f64;
        let prod_us_per_apply = (prod_dur.as_nanos() as f64) / 1000.0 / total_applies;
        let e2e_us_per_apply = (e2e_dur.as_nanos() as f64) / 1000.0 / total_applies;
        let expected = 2 * iters * n as u64;
        println!(
            "{:>5} {:>7} {:>11.2} {:>11.2} {:>11.2} {:>11.2} {:>13} {:>13}",
            n,
            iters,
            prod_dur.as_secs_f64() * 1000.0,
            prod_us_per_apply,
            e2e_dur.as_secs_f64() * 1000.0,
            e2e_us_per_apply,
            observed,
            expected,
        );
    }
}

fn num_cpus() -> usize {
    std::thread::available_parallelism().map_or(4, std::num::NonZeroUsize::get)
}
