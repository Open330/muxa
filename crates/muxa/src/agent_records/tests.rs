use super::*;
use serde_json::json;
use std::fs::File;
use std::io::Write;
use tempfile::TempDir;
use time::format_description::well_known::Rfc3339;

fn fixture() -> (TempDir, RecordStore, PathBuf) {
    let dir = TempDir::new().unwrap();
    let roots = dir.path().join("records");
    fs::create_dir(&roots).unwrap();
    let store = RecordStore::new(dir.path().join("cache/index.sqlite"), vec![roots.clone()]);
    (dir, store, roots)
}

fn write(path: &Path, rows: &[serde_json::Value]) {
    let mut f = File::create(path).unwrap();
    for row in rows {
        writeln!(f, "{row}").unwrap();
    }
}

fn claude(typ: &str, content: serde_json::Value, at: &str) -> serde_json::Value {
    json!({"type":typ,"sessionId":"claude-test","cwd":"/project","timestamp":at,"message":{"role":typ,"content":content}})
}

fn request(query: &str) -> SearchRequest {
    SearchRequest {
        query: query.into(),
        ..Default::default()
    }
}

#[test]
fn discovery_reports_a_gap_when_last_accepted_entry_reaches_limit() {
    let (_dir, _store, roots) = fixture();
    write(&roots.join("a.jsonl"), &[]);
    write(&roots.join("b.jsonl"), &[]);
    let mut seen: HashSet<PathBuf> = (0..MAX_DISCOVERY_ENTRIES - 2)
        .map(|i| PathBuf::from(format!("/unused-discovery-entry-{i}")))
        .collect();
    let mut files = Vec::new();
    let mut coverage = Coverage::default();
    discover(&roots, &mut seen, &mut files, &mut coverage, Instant::now());
    assert_eq!(files.len(), 1);
    assert!(coverage.files_failed > 0);
    assert!(coverage
        .warnings
        .iter()
        .any(|w| w.starts_with("Discovery budget")));
}

#[test]
fn natural_korean_question_preserves_identifier_and_interprets_kst_event_time() {
    let plan = query::plan(
        "2026.10.22 11:54 KST에 callabo-internal 버킷을 우리가 만들었는지",
        "UTC",
        OffsetDateTime::UNIX_EPOCH,
    )
    .unwrap();
    assert!(plan.terms.contains(&"callabo-internal".into()));
    assert!(plan.expanded_terms.contains(&"create_bucket".into()));
    assert_eq!(plan.from.as_deref(), Some("2026-10-22T11:39:00+09:00"));
    assert_eq!(plan.to.as_deref(), Some("2026-10-22T12:09:00+09:00"));
    assert!(!plan.terms.iter().any(|t| t.contains("2026")));
}

#[test]
fn pairing_budget_accounts_for_rows_already_in_context() {
    let (_dir, store, roots) = fixture();
    let mut rows = vec![claude(
        "assistant",
        json!([{"type":"tool_use","id":"many-results","name":"Bash","input":{"command":"pair-budget-anchor"}}]),
        "2026-10-22T02:54:00Z",
    )];
    for i in 0..110 {
        rows.push(claude("user", json!([{"type":"tool_result","tool_use_id":"many-results","content":format!("result {i}")}]), "2026-10-22T02:54:01Z"));
    }
    write(&roots.join("many.jsonl"), &rows);
    let hits = store.search(&request("pair-budget-anchor")).unwrap();
    let result = store
        .read(&ReadRequest {
            event_id: hits.matches[0].event.event_id.clone(),
            before: 0,
            after: 50,
            ..Default::default()
        })
        .unwrap();
    assert_eq!(result.related.len(), 50);
    assert!(result.related_truncated);
}

#[test]
fn korean_am_pm_and_relative_days_are_explicit_in_plan() {
    let now = OffsetDateTime::parse("2026-10-27T02:00:00Z", &Rfc3339).unwrap();
    let p = query::plan("2026년 10월 22일 오후 12시 10분 버킷 생성", "KST", now).unwrap();
    assert_eq!(p.from.as_deref(), Some("2026-10-22T11:55:00+09:00"));
    let p = query::plan("어제 버킷 생성", "KST", now).unwrap();
    assert_eq!(p.from.as_deref(), Some("2026-10-26T00:00:00+09:00"));
    assert!(query::plan("2026-02-30 bucket", "UTC", now).is_err());
    assert!(query::offset("+00:90").is_err());
}

#[test]
fn finds_execution_by_event_date_not_filename_and_reads_separated_tool_pair() {
    let (_dir, store, roots) = fixture();
    let path = roots.join("old-session-2026-06-01.jsonl");
    write(
        &path,
        &[
            claude(
                "user",
                json!("create the callabo-internal bucket"),
                "2026-10-22T02:53:00Z",
            ),
            claude(
                "assistant",
                json!([{"type":"tool_use","id":"tool-1","name":"Bash","input":{"command":"aws --endpoint-url https://minio.rtzr.ai s3api create-bucket --bucket callabo-internal"}}]),
                "2026-10-22T02:54:32Z",
            ),
            claude(
                "assistant",
                json!([{"type":"text","text":"Waiting for the command"}]),
                "2026-10-22T02:54:33Z",
            ),
            claude(
                "user",
                json!([{"type":"tool_result","tool_use_id":"tool-1","content":"created","is_error":false}]),
                "2026-10-22T02:54:34Z",
            ),
            claude(
                "assistant",
                json!([{"type":"text","text":"callabo-internal was created"}]),
                "2026-10-26T02:54:34Z",
            ),
        ],
    );
    let mut req = request("2026.10.22 11:54 KST callabo-internal 버킷 생성");
    req.kinds = vec!["tool_call".into()];
    let result = store.search(&req).unwrap();
    assert!(result.coverage.complete);
    assert_eq!(result.matches.len(), 1);
    let event = &result.matches[0].event;
    assert_eq!(event.line, 2);
    let read = store
        .read(&ReadRequest {
            event_id: event.event_id.clone(),
            before: 0,
            after: 0,
            ..Default::default()
        })
        .unwrap();
    assert_eq!(read.related.len(), 1);
    assert_eq!(read.related[0].kind, "tool_result");
    assert_eq!(read.related[0].is_error, Some(false));
    assert!(read.has_before && read.has_after);
}

#[test]
fn codex_canonical_messages_and_tool_calls_avoid_lifecycle_duplicates() {
    let (_dir, store, roots) = fixture();
    write(
        &roots.join("codex.jsonl"),
        &[
            json!({"type":"session_meta","payload":{"id":"codex-session","cwd":"/repo"}}),
            json!({"timestamp":"2026-10-22T02:54:32Z","type":"response_item","payload":{"type":"custom_tool_call","name":"exec","call_id":"c1","input":"create_bucket('callabo-internal')"}}),
            json!({"timestamp":"2026-10-22T02:54:33Z","type":"event_msg","payload":{"type":"agent_message","message":"callabo-internal done"}}),
            json!({"timestamp":"2026-10-22T02:54:33Z","type":"response_item","payload":{"type":"message","role":"assistant","content":[{"type":"output_text","text":"callabo-internal done"}]}}),
            json!({"timestamp":"2026-10-22T02:54:34Z","type":"response_item","payload":{"type":"custom_tool_call_output","call_id":"c1","output":"{\"exit_code\":0}"}}),
        ],
    );
    let result = store.search(&request("callabo-internal")).unwrap();
    assert_eq!(result.matches.len(), 2);
    assert!(result
        .matches
        .iter()
        .all(|h| h.event.session_id == "codex-session" && h.event.cwd.as_deref() == Some("/repo")));
    let call = result
        .matches
        .iter()
        .find(|h| h.event.kind == "tool_call")
        .unwrap();
    let read = store
        .read(&ReadRequest {
            event_id: call.event.event_id.clone(),
            before: 0,
            after: 0,
            ..Default::default()
        })
        .unwrap();
    assert_eq!(read.related.len(), 1);
}

#[test]
fn copied_tool_logs_stay_user_prose_and_instructions_are_opt_in() {
    let (_dir, store, roots) = fixture();
    write(
        &roots.join("claude.jsonl"),
        &[
            claude(
                "user",
                json!("# AGENTS.md instructions\ncallabo-internal"),
                "2026-10-22T02:00:00Z",
            ),
            claude(
                "user",
                json!(
                    r#"Here is a quote: {"type":"tool_use","command":"create callabo-internal"}"#
                ),
                "2026-10-22T02:01:00Z",
            ),
        ],
    );
    let result = store.search(&request("callabo-internal")).unwrap();
    assert_eq!(result.matches.len(), 1);
    assert_eq!(result.matches[0].event.kind, "user");
    let mut req = request("callabo-internal");
    req.include_instructions = true;
    assert_eq!(store.search(&req).unwrap().matches.len(), 2);
    req.exclude_session_ids = vec!["claude-test".into()];
    assert!(store.search(&req).unwrap().matches.is_empty());
}

#[test]
fn indexing_resumes_with_budget_and_tracks_append_without_duplicates() {
    let (_dir, store, roots) = fixture();
    let path = roots.join("claude.jsonl");
    write(
        &path,
        &[
            claude("user", json!("bucket alpha"), "2026-10-22T02:00:00Z"),
            claude("user", json!("bucket beta"), "2026-10-22T02:01:00Z"),
        ],
    );
    let mut req = request("bucket");
    req.max_index_bytes = 1;
    assert!(!store.search(&req).unwrap().coverage.complete);
    let result = store.search(&req).unwrap();
    assert!(result.coverage.complete);
    assert_eq!(result.matches.len(), 2);
    let result = store.search(&req).unwrap();
    assert_eq!(result.coverage.bytes_read, 0);
    let mut file = fs::OpenOptions::new().append(true).open(path).unwrap();
    writeln!(
        file,
        "{}",
        claude("user", json!("bucket gamma"), "2026-10-22T02:02:00Z")
    )
    .unwrap();
    drop(file);
    req.max_index_bytes = DEFAULT_INDEX_BYTES;
    assert_eq!(store.search(&req).unwrap().matches.len(), 3);
}

#[test]
fn multiline_commands_and_encoded_json_arguments_are_searchable() {
    let (_dir, store, roots) = fixture();
    write(
        &roots.join("commands.jsonl"),
        &[
            claude(
                "assistant",
                json!([{"type":"tool_use","id":"multi","name":"Bash","input":{"command":"cd /tmp\nkubectl rollout restart demo\tgh pr merge 12"}}]),
                "2026-10-22T02:00:00Z",
            ),
            json!({"type":"session_meta","payload":{"id":"codex-multi"}}),
            json!({"type":"response_item","timestamp":"2026-10-22T02:01:00Z","payload":{"type":"function_call","call_id":"multi2","name":"exec_command","arguments":serde_json::to_string(&json!({"cmd":"set -e\naws s3api create-bucket demo"})).unwrap()}}),
        ],
    );
    for term in ["kubectl", "gh", "aws"] {
        let mut r = request("");
        r.terms = vec![term.into()];
        r.kinds = vec!["tool_call".into()];
        let result = store.search(&r).unwrap();
        assert_eq!(result.matches.len(), 1, "{term}");
        assert!(result.coverage.complete);
    }
}

#[test]
fn freeform_code_escapes_are_searchable_without_corrupting_displayed_paths() {
    let (_dir, store, roots) = fixture();
    write(
        &roots.join("freeform.jsonl"),
        &[
            json!({"type":"session_meta","payload":{"id":"freeform"}}),
            json!({"type":"response_item","timestamp":"2026-10-22T02:00:00Z","payload":{"type":"custom_tool_call","call_id":"freeform1","name":"exec","input":r#"text(await tools.exec_command({cmd:"set -e\naws s3api list-buckets"}))"#}}),
            claude("user", json!(r"C:\temp\folder"), "2026-10-22T02:00:01Z"),
        ],
    );
    let r = store.search(&request("aws")).unwrap();
    assert_eq!(r.matches.len(), 1);
    assert!(r.coverage.complete);
    assert!(r.matches[0].event.text.contains(r"\naws"));
    let d = store
        .read(&ReadRequest {
            event_id: r.matches[0].event.event_id.clone(),
            before: 0,
            after: 0,
            ..Default::default()
        })
        .unwrap();
    assert!(d.events[0].text.contains(r"\naws"));
    let r = store.search(&request("temp")).unwrap();
    assert_eq!(r.matches.len(), 1);
    assert_eq!(r.matches[0].event.text, r"C:\temp\folder");
}

#[test]
fn structured_credentials_and_authorization_schemes_never_reach_cache_or_reads() {
    let (dir, store, roots) = fixture();
    let values = [
        "quoted-secret-1234",
        "github-secret-5678",
        "header-secret-9876",
        "structured-secret-4321",
    ];
    write(
        &roots.join("secrets.jsonl"),
        &[
            claude(
                "assistant",
                json!([{"type":"tool_use","id":"credentials","name":"Bash","input":{"command":format!("API_KEY=\"{} with spaces\" GITHUB_TOKEN={} curl -H 'Authorization: token {}' https://example.test",values[0],values[1],values[2]),"client_secret":values[3]}}]),
                "2026-10-22T02:00:00Z",
            ),
            json!({"type":"response_item","timestamp":"2026-10-22T02:00:01Z","payload":{"type":"function_call","name":"exec","call_id":"credentials2","arguments":serde_json::to_string(&json!({"cmd":format!("PASSWORD=\"{}\" run",values[0]),"api_key":values[3]})).unwrap()}}),
        ],
    );
    let r = store.search(&request("curl run")).unwrap();
    assert_eq!(r.matches.len(), 2);
    let mut outputs = serde_json::to_string(&r).unwrap();
    for hit in r.matches {
        outputs.push_str(
            &serde_json::to_string(
                &store
                    .read(&ReadRequest {
                        event_id: hit.event.event_id,
                        ..Default::default()
                    })
                    .unwrap(),
            )
            .unwrap(),
        );
    }
    outputs.push_str(&String::from_utf8_lossy(
        &fs::read(dir.path().join("cache/index.sqlite")).unwrap(),
    ));
    for value in values {
        assert!(!outputs.contains(value), "credential persisted: {value}");
    }
    let escaped = parse::redact(
        r#"PASSWORD=\"escaped-sensitive-value\" GITHUB_TOKEN=another-sensitive-value"#,
    );
    assert!(!escaped.contains("escaped-sensitive-value"));
    assert!(!escaped.contains("another-sensitive-value"));
}

#[test]
fn middle_rewrite_and_append_replaces_cached_evidence() {
    let (_dir, store, roots) = fixture();
    let path = roots.join("middle.jsonl");
    let mut rows: Vec<_> = (0..1000)
        .map(|i| {
            claude(
                "user",
                json!(format!("filler-{i} {}", "x".repeat(100))),
                "2026-10-22T02:00:00Z",
            )
        })
        .collect();
    rows[500] = claude("user", json!("middle-old-marker"), "2026-10-22T02:00:00Z");
    write(&path, &rows);
    assert_eq!(
        store
            .search(&request("middle-old-marker"))
            .unwrap()
            .matches
            .len(),
        1
    );
    rows[500] = claude("user", json!("middle-new-marker"), "2026-10-22T02:00:00Z");
    rows.push(claude("user", json!("appended"), "2026-10-22T02:01:00Z"));
    write(&path, &rows);
    assert!(store
        .search(&request("middle-old-marker"))
        .unwrap()
        .matches
        .is_empty());
    let result = store.search(&request("middle-new-marker")).unwrap();
    assert_eq!(result.matches.len(), 1);
    assert!(result.coverage.complete);
}

#[test]
fn newline_added_to_valid_tail_does_not_shift_source_lines() {
    let (_dir, store, roots) = fixture();
    let path = roots.join("tail-lines.jsonl");
    fs::write(
        &path,
        claude("user", json!("tail-first"), "2026-10-22T02:00:00Z").to_string(),
    )
    .unwrap();
    assert!(
        store
            .search(&request("tail-first"))
            .unwrap()
            .coverage
            .complete
    );
    let mut f = fs::OpenOptions::new().append(true).open(path).unwrap();
    writeln!(f).unwrap();
    writeln!(
        f,
        "{}",
        claude("user", json!("tail-second"), "2026-10-22T02:01:00Z")
    )
    .unwrap();
    drop(f);
    let result = store.search(&request("tail-second")).unwrap();
    assert!(result.coverage.complete);
    assert_eq!(result.matches[0].event.line, 2);
}

#[test]
fn unknown_nested_payloads_are_reported_as_coverage_gaps() {
    let (_dir, store, roots) = fixture();
    write(
        &roots.join("new-wire.jsonl"),
        &[
            json!({"type":"response_item","timestamp":"2026-10-22T02:00:00Z","payload":{"type":"local_shell_call","command":"create invisible-bucket"}}),
            claude(
                "user",
                json!([{"type":"future_text","text":"invisible-bucket"}]),
                "2026-10-22T02:00:01Z",
            ),
        ],
    );
    let result = store.search(&request("invisible-bucket")).unwrap();
    assert!(result.matches.is_empty());
    assert!(!result.coverage.complete);
    assert_eq!(result.coverage.unsupported_records, 2);
}

#[test]
fn moving_a_source_does_not_duplicate_tool_evidence() {
    let (_dir, store, roots) = fixture();
    let old = roots.join("session.jsonl");
    write(
        &old,
        &[
            claude(
                "assistant",
                json!([{"type":"tool_use","id":"archived-call","name":"Bash","input":{"command":"create archived-bucket"}}]),
                "2026-10-22T02:00:00Z",
            ),
            claude(
                "user",
                json!([{"type":"tool_result","tool_use_id":"archived-call","content":"archived-bucket created","is_error":false}]),
                "2026-10-22T02:00:01Z",
            ),
        ],
    );
    store.search(&request("archived-bucket")).unwrap();
    let dir = roots.join("archived");
    fs::create_dir(&dir).unwrap();
    fs::rename(&old, dir.join("session.jsonl")).unwrap();
    let result = store.search(&request("archived-bucket")).unwrap();
    assert_eq!(result.matches.len(), 2);
    let anchor = result
        .matches
        .into_iter()
        .find(|h| h.event.kind == "tool_result")
        .unwrap();
    let read = store
        .read(&ReadRequest {
            event_id: anchor.event.event_id,
            before: 0,
            after: 0,
            ..Default::default()
        })
        .unwrap();
    assert_eq!(read.related.len(), 1);
    assert!(Path::new(&read.related[0].path).exists());
}

#[test]
fn simultaneous_indexers_share_consistent_checkpoints() {
    let (dir, store, roots) = fixture();
    let path = roots.join("concurrent.jsonl");
    let rows: Vec<_> = (0..3000)
        .map(|i| claude("user", json!(format!("word-{i}")), "2026-10-22T02:00:00Z"))
        .collect();
    write(&path, &rows);
    let barrier = std::sync::Arc::new(std::sync::Barrier::new(3));
    let mut tasks = Vec::new();
    for _ in 0..3 {
        let cache = dir.path().join("cache/index.sqlite");
        let roots = roots.clone();
        let barrier = barrier.clone();
        tasks.push(std::thread::spawn(move || {
            barrier.wait();
            RecordStore::new(cache, vec![roots]).search(&request("word-42"))
        }));
    }
    for task in tasks {
        let r = task.join().unwrap().unwrap();
        assert_eq!(r.coverage.files_failed, 0);
        assert!(r.coverage.complete);
    }
    let r = store.search(&request("word-42")).unwrap();
    assert_eq!(r.matches.len(), 11); // FTS prefix: 42 and 420..429
    assert_eq!(r.coverage.malformed_lines, 0);
    let mut append = fs::OpenOptions::new().append(true).open(&path).unwrap();
    writeln!(append, "invalid JSON").unwrap();
    drop(append);
    let barrier = std::sync::Arc::new(std::sync::Barrier::new(3));
    let mut tasks = Vec::new();
    for _ in 0..3 {
        let cache = dir.path().join("cache/index.sqlite");
        let roots = roots.clone();
        let barrier = barrier.clone();
        tasks.push(std::thread::spawn(move || {
            barrier.wait();
            RecordStore::new(cache, vec![roots]).search(&request("word-42"))
        }));
    }
    for task in tasks {
        let r = task.join().unwrap().unwrap();
        assert_eq!(r.coverage.malformed_lines, 1);
        assert_eq!(r.coverage.files_failed, 0);
    }
}

#[test]
fn legacy_unredacted_cache_is_retired_before_a_new_index_is_used() {
    let (dir, mut store, roots) = fixture();
    let legacy = dir.path().join("legacy.sqlite");
    for suffix in ["", "-wal", "-shm", "-journal"] {
        fs::write(
            format!("{}{suffix}", legacy.display()),
            "synthetic old cache secret",
        )
        .unwrap();
    }
    store.legacy_cache = Some(legacy.clone());
    write(
        &roots.join("fresh.jsonl"),
        &[claude(
            "user",
            json!("new-cache-evidence"),
            "2026-10-22T02:00:00Z",
        )],
    );
    assert_eq!(
        store
            .search(&request("new-cache-evidence"))
            .unwrap()
            .matches
            .len(),
        1
    );
    for suffix in ["", "-wal", "-shm", "-journal"] {
        assert!(!Path::new(&format!("{}{suffix}", legacy.display())).exists());
    }
}

#[test]
fn oversized_growing_record_resumes_skipping_without_false_malformed_rows() {
    let (_dir, store, roots) = fixture();
    let path = roots.join("oversize.jsonl");
    let mut f = File::create(&path).unwrap();
    writeln!(
        f,
        "{}",
        claude("user", json!("before-large-record"), "2026-10-22T02:00:00Z")
    )
    .unwrap();
    write!(
        f,
        "{}",
        claude(
            "user",
            json!("x".repeat(MAX_LINE + 1024)),
            "2026-10-22T02:00:01Z"
        )
    )
    .unwrap();
    drop(f);
    let r = store.search(&request("before-large-record")).unwrap();
    assert_eq!(r.coverage.oversized_lines, 1);
    assert!(!r.coverage.complete);
    assert_eq!(r.coverage.malformed_lines, 0);
    let mut f = fs::OpenOptions::new().append(true).open(&path).unwrap();
    writeln!(f).unwrap();
    writeln!(
        f,
        "{}",
        claude("user", json!("after-large-record"), "2026-10-22T02:00:02Z")
    )
    .unwrap();
    drop(f);
    let r = store.search(&request("after-large-record")).unwrap();
    assert_eq!(r.matches.len(), 1);
    assert_eq!(r.matches[0].event.line, 3);
    assert_eq!(r.coverage.oversized_lines, 1);
    assert_eq!(r.coverage.malformed_lines, 0);
}

#[test]
fn changed_large_prefix_with_tiny_budget_rebuilds_and_eventually_finishes() {
    let (_dir, store, roots) = fixture();
    let path = roots.join("rebuild.jsonl");
    let mut rows = vec![
        claude("user", json!("stale-marker"), "2026-10-22T02:00:00Z"),
        claude("user", json!("other-row"), "2026-10-22T02:01:00Z"),
    ];
    write(&path, &rows);
    store.search(&request("stale-marker")).unwrap();
    rows[0] = claude("user", json!("fresh-marker"), "2026-10-22T02:00:00Z");
    rows.push(claude("user", json!("append-row"), "2026-10-22T02:02:00Z"));
    write(&path, &rows);
    let mut req = request("stale-marker fresh-marker");
    req.max_index_bytes = 1;
    let first = store.search(&req).unwrap();
    assert!(!first.coverage.complete);
    assert!(first
        .matches
        .iter()
        .all(|h| !h.event.text.contains("stale-marker")));
    let mut current = false;
    for _ in 0..4 {
        if store.search(&req).unwrap().coverage.complete {
            current = true;
            break;
        }
    }
    assert!(current);
    assert_eq!(
        store
            .search(&request("fresh-marker"))
            .unwrap()
            .matches
            .len(),
        1
    );
}

#[test]
fn explicit_instruction_kind_includes_instruction_events() {
    let (_dir, store, roots) = fixture();
    write(
        &roots.join("instructions.jsonl"),
        &[claude(
            "user",
            json!("<system-reminder>kubectl instruction</system-reminder>"),
            "2026-10-22T02:00:00Z",
        )],
    );
    assert!(store
        .search(&request("kubectl"))
        .unwrap()
        .matches
        .is_empty());
    let mut req = request("kubectl");
    req.kinds = vec!["instruction".into()];
    let r = store.search(&req).unwrap();
    assert_eq!(r.matches.len(), 1);
    assert!(r.coverage.complete);
}

#[test]
fn incomplete_tail_is_retried_and_malformed_lines_are_reported() {
    let (_dir, store, roots) = fixture();
    let path = roots.join("claude.jsonl");
    let event = claude("user", json!("bucket"), "2026-10-22T02:00:00Z").to_string();
    fs::write(&path, &event[..event.len() / 2]).unwrap();
    assert!(!store.search(&request("bucket")).unwrap().coverage.complete);
    let mut f = fs::OpenOptions::new().append(true).open(path).unwrap();
    writeln!(f, "{}", &event[event.len() / 2..]).unwrap();
    writeln!(f, "not valid json").unwrap();
    drop(f);
    let result = store.search(&request("bucket")).unwrap();
    assert_eq!(result.matches.len(), 1);
    assert_eq!(result.coverage.malformed_lines, 1);
    assert!(!result.coverage.complete);
    assert_eq!(
        store
            .search(&request("bucket"))
            .unwrap()
            .coverage
            .malformed_lines,
        1
    );
}

#[test]
fn source_rewrite_removes_stale_search_hits() {
    let (_dir, store, roots) = fixture();
    let path = roots.join("claude.jsonl");
    write(
        &path,
        &[claude(
            "user",
            json!("bucket original"),
            "2026-10-22T02:00:00Z",
        )],
    );
    assert_eq!(store.search(&request("original")).unwrap().matches.len(), 1);
    write(
        &path,
        &[claude("user", json!("replacement"), "2026-10-22T02:00:00Z")],
    );
    assert!(store
        .search(&request("original"))
        .unwrap()
        .matches
        .is_empty());
    assert_eq!(
        store.search(&request("replacement")).unwrap().matches.len(),
        1
    );
}

#[cfg(unix)]
#[test]
fn symlinked_profile_roots_are_deduplicated() {
    let (dir, _, roots) = fixture();
    write(
        &roots.join("claude.jsonl"),
        &[claude("user", json!("bucket"), "2026-10-22T02:00:00Z")],
    );
    let alias = dir.path().join("alias");
    std::os::unix::fs::symlink(&roots, &alias).unwrap();
    let store = RecordStore::new(dir.path().join("index.sqlite"), vec![roots, alias]);
    let result = store.search(&request("bucket")).unwrap();
    assert_eq!(result.coverage.files_discovered, 1);
    assert_eq!(result.matches.len(), 1);
}

#[test]
fn secrets_are_redacted_in_search_read_and_cache() {
    let (dir, store, roots) = fixture();
    write(&roots.join("claude.jsonl"),&[claude("user",json!("bucket AWS_SECRET_ACCESS_KEY=verysecret https://minio/bucket?X-Amz-Signature=abcdef&X-Amz-Credential=secret"),"2026-10-22T02:00:00Z")]);
    let result = store.search(&request("bucket")).unwrap();
    let read = store
        .read(&ReadRequest {
            event_id: result.matches[0].event.event_id.clone(),
            ..Default::default()
        })
        .unwrap();
    let text = serde_json::to_string(&read).unwrap();
    assert!(!text.contains("verysecret"));
    assert!(!text.contains("abcdef"));
    let raw = fs::read(dir.path().join("cache/index.sqlite")).unwrap();
    assert!(!String::from_utf8_lossy(&raw).contains("verysecret"));
}

#[test]
fn read_clipped_unicode_text_continues_by_character_offset() {
    let (_dir, store, roots) = fixture();
    write(
        &roots.join("claude.jsonl"),
        &[claude(
            "user",
            json!("버킷 생성 결과를 검토합니다"),
            "2026-10-22T02:00:00Z",
        )],
    );
    let hit = store.search(&request("버킷")).unwrap().matches.remove(0);
    let mut req = ReadRequest {
        event_id: hit.event.event_id,
        before: 0,
        after: 0,
        max_chars: 10,
        ..Default::default()
    };
    let first = store.read(&req).unwrap();
    assert_eq!(first.events[0].text, "버킷 생성");
    req.text_offset = first.next_text_offset.unwrap();
    let next = store.read(&req).unwrap();
    assert_eq!(next.events[0].text, " 결과를 ");
}

#[test]
fn filters_and_fts_input_are_validated_without_query_injection() {
    let (_dir, store, roots) = fixture();
    write(
        &roots.join("claude.jsonl"),
        &[claude("user", json!("bucket"), "2026-10-22T02:00:00Z")],
    );
    let mut req = request("bucket");
    req.from = Some("2026-10-23T00:00:00Z".into());
    assert!(store.search(&req).unwrap().matches.is_empty());
    req.to = Some("2026-10-22T00:00:00Z".into());
    assert!(store.search(&req).is_err());
    let mut req = request("bucket");
    req.terms = vec!["\" OR text:* --".into()];
    assert!(store.search(&req).is_ok());
    req.providers = vec!["unknown".into()];
    assert!(store.search(&req).is_err());
}

#[test]
fn bearer_and_quoted_credentials_are_not_kept_in_evidence() {
    let text = parse::redact(
        r#"{"Authorization":"Bearer topsecret.token", "PASSWORD": "secret with spaces"} API_KEY='another secret'"#,
    );
    for secret in ["topsecret", "with spaces", "another secret"] {
        assert!(!text.contains(secret), "{text}");
    }
}

#[test]
fn valid_json_without_final_newline_and_unknown_timestamps_remain_searchable() {
    let (_dir, store, roots) = fixture();
    let value = claude("user", json!("bucket"), "not-a-timestamp");
    fs::write(roots.join("tail.jsonl"), value.to_string()).unwrap();
    let result = store.search(&request("bucket")).unwrap();
    assert_eq!(result.matches.len(), 1);
    assert!(result.coverage.complete);
    assert_eq!(result.coverage.untimed_events, 1);
    let mut r = request("bucket");
    r.from = Some("2026-10-22T00:00:00Z".into());
    let result = store.search(&r).unwrap();
    assert!(result.matches.is_empty());
    assert!(!result.coverage.complete);
}

#[test]
fn tool_result_binary_blocks_are_never_indexed() {
    let mut ctx = parse::Context::default();
    let v = claude(
        "user",
        json!([{"type":"tool_result","tool_use_id":"t","content":[
            {"type":"image","source":{"data":"privatebinary"}},
            {"type":"text","text":"bucket command result"}
        ]}]),
        "2026-10-22T00:00:00Z",
    );
    let events = parse::events(&v, &mut ctx);
    assert_eq!(events[0].text, "bucket command result");
}

#[test]
fn multiple_dates_do_not_silently_narrow_to_the_first_day() {
    let plan = query::plan(
        "2026-10-21부터 2026-10-23까지 bucket 생성",
        "UTC",
        OffsetDateTime::UNIX_EPOCH,
    )
    .unwrap();
    assert!(plan.from.is_none());
    assert!(plan.to.is_none());
    assert!(plan.notes.iter().any(|n| n.contains("Multiple dates")));
    let plan = query::plan("backstage deploy", "UTC", OffsetDateTime::UNIX_EPOCH).unwrap();
    assert_eq!(plan.timezone, "UTC");
    let plan = query::plan(
        "callabo-internal이라는 버킷을 만들었나요",
        "UTC",
        OffsetDateTime::UNIX_EPOCH,
    )
    .unwrap();
    assert!(plan.terms.contains(&"callabo-internal".into()));
}

#[test]
fn continuously_appended_sources_progress_with_a_small_shared_budget() {
    let (_dir, store, roots) = fixture();
    let paths = [roots.join("a.jsonl"), roots.join("b.jsonl")];
    for (i, path) in paths.iter().enumerate() {
        let mut rows = (0..40)
            .map(|j| {
                claude(
                    "user",
                    json!(format!("ordinary {i} row {j}")),
                    "2026-10-22T02:00:00Z",
                )
            })
            .collect::<Vec<_>>();
        rows.push(claude(
            "user",
            json!(format!("growing-tail-marker-{i}")),
            "2026-10-22T02:00:01Z",
        ));
        write(path, &rows);
    }
    let mut req = request("growing-tail-marker");
    req.max_index_bytes = 800;
    let mut reached = false;
    for round in 0..80 {
        for path in &paths {
            let mut file = fs::OpenOptions::new().append(true).open(path).unwrap();
            writeln!(
                file,
                "{}",
                claude(
                    "user",
                    json!(format!("tick-{round}")),
                    "2026-10-22T02:00:02Z"
                )
            )
            .unwrap();
        }
        if store.search(&req).unwrap().matches.len() == 2 {
            reached = true;
            break;
        }
    }
    assert!(
        reached,
        "both growing files must reach their original tail without restarting"
    );
}

#[test]
fn verification_timeout_keeps_existing_events_and_checkpoint() {
    let (_dir, store, roots) = fixture();
    let path = roots.join("timeout.jsonl");
    write(
        &path,
        &[claude(
            "user",
            json!("preserved-evidence"),
            "2026-10-22T02:00:00Z",
        )],
    );
    let first = store.search(&request("preserved-evidence")).unwrap();
    let id = &first.matches[0].event.event_id;
    let mut file = fs::OpenOptions::new().append(true).open(&path).unwrap();
    writeln!(
        file,
        "{}",
        claude("user", json!("new-evidence"), "2026-10-22T02:00:01Z")
    )
    .unwrap();
    drop(file);
    let mut conn = store.connect().unwrap();
    let prior: u64 = conn
        .query_row("SELECT offset FROM sources", [], |r| r.get(0))
        .unwrap();
    let progress = index_file(
        &mut conn,
        &path,
        1,
        Instant::now().checked_sub(Duration::from_secs(13)).unwrap(),
    )
    .unwrap();
    assert!(!progress.current);
    assert_eq!(progress.index_bytes, 0);
    assert_eq!(
        conn.query_row("SELECT offset FROM sources", [], |r| r.get::<_, u64>(0))
            .unwrap(),
        prior
    );
    assert_eq!(
        conn.query_row("SELECT count(*) FROM events WHERE id=?", [id], |r| r
            .get::<_, u64>(0))
            .unwrap(),
        1
    );
    assert!(!source_current(&conn, &path).unwrap());
}

#[test]
fn escaped_keys_headers_url_passwords_and_env_pairs_are_redacted() {
    let (dir, store, roots) = fixture();
    let cases = [
        (
            r#"{\"password\": \"pwLEAK6\", \"api_key\": \"akLEAK7\"}"#,
            vec!["pwLEAK6", "akLEAK7"],
        ),
        (
            r"curl -H 'X-API-Key: apikeyLEAK1' https://service",
            vec!["apikeyLEAK1"],
        ),
        ("https://bob:pwLEAK3@host/path", vec!["pwLEAK3"]),
        ("mysql -h localhost -ppwLEAK4 -e SELECT", vec!["pwLEAK4"]),
        (
            r#"env=[{"name":"DB_PASSWORD","value":"pwLEAK8"}]"#,
            vec!["pwLEAK8"],
        ),
        (
            r#"env=[{\"name\":\"DB_PASSWORD\",\"value\":\"pwLEAK9\"}]"#,
            vec!["pwLEAK9"],
        ),
        (
            r#"[{"name":"DB_PASSWORD","value":"pwLEAK10"}]"#,
            vec!["pwLEAK10"],
        ),
    ];
    let rows=cases.iter().map(|(text,_)|claude("user",json!([{"type":"tool_result","tool_use_id":"credentials","content":format!("credential-probe {text}")}]),"2026-10-22T02:00:00Z")).collect::<Vec<_>>();
    write(&roots.join("credentials.jsonl"), &rows);
    let results = store.search(&request("credential-probe")).unwrap();
    assert_eq!(results.matches.len(), cases.len());
    let mut output = serde_json::to_string(&results).unwrap();
    for hit in &results.matches {
        output.push_str(
            &serde_json::to_string(
                &store
                    .read(&ReadRequest {
                        event_id: hit.event.event_id.clone(),
                        ..Default::default()
                    })
                    .unwrap(),
            )
            .unwrap(),
        );
    }
    let cache = fs::read(dir.path().join("cache/index.sqlite")).unwrap();
    let cache = String::from_utf8_lossy(&cache);
    for (_, secrets) in cases {
        for secret in secrets {
            assert!(!output.contains(secret), "output leaked {secret}");
            assert!(!cache.contains(secret), "cache leaked {secret}");
        }
    }
}

#[test]
fn known_metadata_is_excluded_but_unknown_content_still_reports_a_gap() {
    for typ in [
        "pr-link",
        "frame-link",
        "cost-state",
        "isolation-latch",
        "tools-used",
    ] {
        assert_eq!(parse::unsupported(&json!({"type":typ})), 0, "{typ}");
    }
    for typ in [
        "patch_apply_begin",
        "patch_apply_end",
        "entered_review_mode",
        "exited_review_mode",
    ] {
        assert_eq!(
            parse::unsupported(&json!({"type":"event_msg","payload":{"type":typ}})),
            0,
            "{typ}"
        );
    }
    assert_eq!(
        parse::unsupported(&claude(
            "user",
            json!([{"type":"tool_result","content":[{"type":"tool_reference","tool_name":"example"}]}]),
            "2026-10-22T02:00:00Z"
        )),
        0
    );
    for typ in ["web_search_call", "unrecognized_content"] {
        assert_eq!(
            parse::unsupported(&json!({"type":"response_item","payload":{"type":typ}})),
            1
        );
    }
}

#[test]
fn tool_result_artifact_directories_are_not_discovered_as_transcripts() {
    let (_dir, store, roots) = fixture();
    write(
        &roots.join("session.jsonl"),
        &[claude(
            "user",
            json!("discovery-probe"),
            "2026-10-22T02:00:00Z",
        )],
    );
    let artifacts = roots.join("session/tool-results");
    fs::create_dir_all(&artifacts).unwrap();
    fs::write(artifacts.join("copied.jsonl"), "not a transcript").unwrap();
    let result = store.search(&request("discovery-probe")).unwrap();
    assert_eq!(result.coverage.files_discovered, 1);
    assert!(result.coverage.complete);
}

#[test]
fn multiple_clocks_keep_the_whole_day_and_report_ambiguity() {
    for text in [
        "2026.10.22 11:00~12:00 사이 bucket",
        "2026.10.22 오전 11시 오후 1시 bucket",
    ] {
        let p = query::plan(text, "KST", OffsetDateTime::UNIX_EPOCH).unwrap();
        assert_eq!(p.from.as_deref(), Some("2026-10-22T00:00:00+09:00"));
        assert_eq!(p.to.as_deref(), Some("2026-10-23T00:00:00+09:00"));
        assert!(p.notes.iter().any(|n| n.starts_with("Multiple clocks")));
    }
}

#[test]
fn redaction_upgrade_rebuilds_and_purges_previously_cached_plaintext() {
    let (dir, store, roots) = fixture();
    let path = roots.join("upgrade.jsonl");
    write(
        &path,
        &[claude(
            "user",
            json!("upgrade-probe X-API-Key: upgrade-secret-marker"),
            "2026-10-22T02:00:00Z",
        )],
    );
    let mut conn = store.connect().unwrap();
    index_file(&mut conn, &path, DEFAULT_INDEX_BYTES, Instant::now()).unwrap();
    // Emulate the older schema's pre-fix plaintext cache, including freed pages.
    conn.execute(
        "UPDATE events SET text='upgrade-secret-marker',metadata='upgrade-secret-marker'",
        [],
    )
    .unwrap();
    conn.pragma_update(None, "user_version", 3).unwrap();
    drop(conn);
    assert!(
        String::from_utf8_lossy(&fs::read(dir.path().join("cache/index.sqlite")).unwrap())
            .contains("upgrade-secret-marker")
    );
    let result = store.search(&request("upgrade-probe")).unwrap();
    assert_eq!(result.matches.len(), 1);
    let data = fs::read(dir.path().join("cache/index.sqlite")).unwrap();
    assert!(!String::from_utf8_lossy(&data).contains("upgrade-secret-marker"));
    let conn = store.connect().unwrap();
    let version: u32 = conn
        .query_row("PRAGMA user_version", [], |r| r.get(0))
        .unwrap();
    assert_eq!(version, CACHE_SCHEMA_VERSION);
    conn.pragma_update(None, "user_version", CACHE_SCHEMA_VERSION + 1)
        .unwrap();
    drop(conn);
    assert!(store.connect().is_err());
}
