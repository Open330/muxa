//! Live, user-facing messages from structured CLI output. Tool payloads and
//! reasoning never become progress; exit status and the result envelope still
//! decide the terminal outcome.
use super::{
    non_zero_detail, parse_claude_json, parse_codex_jsonl, timeout_message, AskAnswer, AskEngine,
};
use serde_json::Value;
use std::process::Stdio;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, BufReader};
use tokio::sync::watch;

const OUTPUT_LIMIT: usize = 32 * 1024 * 1024;
const PROGRESS_LIMIT: usize = 32 * 1024;
const STDERR_LIMIT: usize = 64 * 1024;

pub(super) async fn run(
    mut command: tokio::process::Command,
    engine: AskEngine,
    program: &str,
    timeout: Duration,
    progress: Option<watch::Sender<String>>,
) -> Result<AskAnswer, String> {
    command.stdout(Stdio::piped()).stderr(Stdio::piped());
    let mut child = command
        .spawn()
        .map_err(|error| format!("spawning {program}: {error}"))?;
    let stdout = child.stdout.take().ok_or("missing CLI stdout")?;
    let mut stderr = child.stderr.take().ok_or("missing CLI stderr")?;
    if let Some(progress) = &progress {
        progress.send_replace("Starting the provider…".to_string());
    }
    let exchange = async {
        let read_stdout = async {
            let mut lines = BufReader::new(stdout.take((OUTPUT_LIMIT + 1) as u64)).lines();
            let mut raw = String::new();
            let mut visible = String::new();
            let mut last_result = None;
            while let Some(line) = lines.next_line().await.map_err(|error| error.to_string())? {
                if raw.len() + line.len() + 1 > OUTPUT_LIMIT {
                    return Err("ask CLI output exceeded 32 MiB".to_string());
                }
                raw.push_str(&line);
                raw.push('\n');
                if let Ok(event) = serde_json::from_str::<Value>(&line) {
                    if engine == AskEngine::Claude && event["type"] == "result" {
                        last_result = Some(line.clone());
                    }
                    if let Some(text) = message(engine, &event) {
                        if !visible.is_empty() {
                            visible.push_str("\n\n");
                        }
                        visible.push_str(&text);
                        trim_front(&mut visible, PROGRESS_LIMIT);
                        if let Some(progress) = &progress {
                            progress.send_replace(visible.clone());
                        }
                    }
                }
            }
            Ok::<_, String>((raw, last_result))
        };
        let read_stderr = async {
            let mut kept = Vec::new();
            let mut buffer = [0; 8192];
            loop {
                let count = stderr
                    .read(&mut buffer)
                    .await
                    .map_err(|error| error.to_string())?;
                if count == 0 {
                    break;
                }
                kept.extend_from_slice(&buffer[..count]);
                if kept.len() > STDERR_LIMIT {
                    kept.drain(..kept.len() - STDERR_LIMIT);
                }
            }
            Ok::<_, String>(String::from_utf8_lossy(&kept).into_owned())
        };
        let wait = async { child.wait().await.map_err(|error| error.to_string()) };
        let ((raw, result), stderr, status) = tokio::try_join!(read_stdout, read_stderr, wait)?;
        if !status.success() {
            return Err(format!(
                "{program} exited non-zero: {}",
                non_zero_detail(result.as_deref().unwrap_or(&raw), &stderr)
            ));
        }
        match engine {
            AskEngine::Codex => parse_codex_jsonl(&raw),
            AskEngine::Claude => parse_claude_json(
                result
                    .as_deref()
                    .ok_or("claude stream ended without a result")?,
            ),
            _ => unreachable!("only structured streaming CLIs use this reader"),
        }
    };
    tokio::time::timeout(timeout, exchange)
        .await
        .map_err(|_| timeout_message(program, timeout))?
}

fn trim_front(text: &mut String, limit: usize) {
    if text.len() > limit {
        let mut start = text.len() - limit;
        while !text.is_char_boundary(start) {
            start += 1;
        }
        text.drain(..start);
    }
}

fn message(engine: AskEngine, event: &Value) -> Option<String> {
    let text = match engine {
        AskEngine::Codex
            if event["type"] == "item.completed" && event["item"]["type"] == "agent_message" =>
        {
            event["item"]["text"].as_str()?.to_owned()
        }
        AskEngine::Claude
            if event["type"] == "assistant" && event["message"]["role"] == "assistant" =>
        {
            event["message"]["content"]
                .as_array()?
                .iter()
                .filter(|part| part["type"] == "text")
                .filter_map(|part| part["text"].as_str())
                .collect::<Vec<_>>()
                .join("\n")
        }
        _ => return None,
    };
    (!text.trim().is_empty()).then_some(text)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn progress_excludes_tools_reasoning_and_errors() {
        for item in ["command_execution", "reasoning", "mcp_tool_call"] {
            assert!(message(
                AskEngine::Codex,
                &json!({"type":"item.completed","item":{"type":item,"text":"private"}})
            )
            .is_none());
        }
        assert!(message(
            AskEngine::Codex,
            &json!({"type":"error","message":"private"})
        )
        .is_none());
        assert_eq!(
            message(
                AskEngine::Codex,
                &json!({"type":"item.completed","item":{"type":"agent_message","text":"Working"}})
            )
            .as_deref(),
            Some("Working")
        );
        assert_eq!(message(AskEngine::Claude, &json!({"type":"assistant","message":{"role":"assistant","content":[{"type":"thinking","thinking":"private"},{"type":"tool_use","name":"secret"},{"type":"text","text":"Working"}]}})).as_deref(), Some("Working"));
    }

    #[test]
    fn progress_truncation_preserves_unicode() {
        let mut text = "한글".repeat(PROGRESS_LIMIT);
        trim_front(&mut text, PROGRESS_LIMIT);
        assert!(text.len() <= PROGRESS_LIMIT);
        assert!(text.ends_with("한글"));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn store_publishes_progress_without_persisting_it_or_advancing_history() {
        use super::super::{AskOptions, AskStatus, AskStore};
        use crate::config::AskProviderConfig;
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let executable = dir.path().join("codex");
        std::fs::write(
            &executable,
            r#"#!/bin/sh
printf '%s\n' '{"type":"thread.started","thread_id":"store-session"}'
printf '%s\n' '{"type":"item.completed","item":{"type":"agent_message","text":"Checking"}}'
while ! [ -f "$(dirname "$0")/continue" ]; do sleep 0.01; done
printf '%s\n' '{"type":"item.completed","item":{"type":"agent_message","text":"Done"}}'
"#,
        )
        .unwrap();
        std::fs::set_permissions(&executable, std::fs::Permissions::from_mode(0o700)).unwrap();
        let path = dir.path().join("ask.json");
        let store = AskStore::load(AskOptions {
            enabled: true,
            agent: "codex".into(),
            path: Some(path.clone()),
            providers: std::collections::BTreeMap::from([(
                "codex".into(),
                AskProviderConfig {
                    executable: Some(executable.display().to_string()),
                    ..Default::default()
                },
            )]),
            ..Default::default()
        })
        .await;
        let mut revisions = store.subscribe();
        let pending = store.ask("hello").await.unwrap();
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                revisions.changed().await.unwrap();
                let entries = store.list().await;
                let entry = entries.iter().find(|entry| entry.id == pending.id).unwrap();
                if entry.answer == "Checking" {
                    assert_eq!(entry.status, AskStatus::Running);
                    assert!(entry.answered_at.is_none());
                    assert_eq!(entry.prompt, "hello");
                    assert!(super::super::replay_history(
                        &entries,
                        pending.conversation_id.as_deref().unwrap()
                    )
                    .is_empty());
                    let disk: Value =
                        serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
                    assert_eq!(disk["entries"][0]["answer"], "");
                    break;
                }
            }
        })
        .await
        .unwrap();
        std::fs::write(dir.path().join("continue"), "").unwrap();
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                revisions.changed().await.unwrap();
                let entries = store.list().await;
                let entry = entries.iter().find(|entry| entry.id == pending.id).unwrap();
                if entry.status == AskStatus::Answered {
                    assert_eq!(entry.answer, "Done");
                    assert_eq!(entry.agent_session_id.as_deref(), Some("store-session"));
                    break;
                }
            }
        })
        .await
        .unwrap();
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn messages_arrive_before_exit_and_do_not_complete_the_turn() {
        let mut cmd = tokio::process::Command::new("/bin/sh");
        cmd.kill_on_drop(true).args(["-c", r#"
printf '%s\n' '{"type":"thread.started","thread_id":"test-session"}'
printf '%s\n' '{"type":"item.completed","item":{"type":"agent_message","text":"Checking"}}'
sleep 1
printf '%s\n' '{"type":"item.completed","item":{"type":"agent_message","text":"Done"}}'
printf '%s\n' '{"type":"item.completed","item":{"type":"command_execution","text":"private output"}}'
"#]);
        let (sender, mut receiver) = watch::channel(String::new());
        let execution = run(
            cmd,
            AskEngine::Codex,
            "fake-codex",
            Duration::from_secs(5),
            Some(sender),
        );
        tokio::pin!(execution);
        tokio::select! {
            result = &mut execution => panic!("completed before progress: {result:?}"),
            () = async {
                loop {
                    receiver.changed().await.unwrap();
                    if receiver.borrow_and_update().contains("Checking") { break; }
                }
            } => {}
        }
        let answer = execution.await.unwrap();
        assert_eq!(answer.text, "Done");
        assert_eq!(answer.session_id.as_deref(), Some("test-session"));
        assert!(!receiver.borrow().contains("private output"));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn nonzero_exit_is_failed_even_after_an_assistant_message() {
        let mut cmd = tokio::process::Command::new("/bin/sh");
        cmd.kill_on_drop(true).args(["-c", r#"printf '%s\n' '{"type":"item.completed","item":{"type":"agent_message","text":"Working"}}'; echo 'authentication failed' >&2; exit 1"#]);
        let error = run(
            cmd,
            AskEngine::Codex,
            "fake-codex",
            Duration::from_secs(5),
            None,
        )
        .await
        .unwrap_err();
        assert!(error.contains("authentication failed"));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn claude_stream_requires_the_final_result_envelope() {
        for (result, success) in [
            (
                r#"{"type":"result","result":"Answer","session_id":"claude-session","total_cost_usd":0.1}"#,
                true,
            ),
            ("", false),
        ] {
            let mut cmd = tokio::process::Command::new("/bin/sh");
            cmd.kill_on_drop(true).args(["-c", r#"printf '%s\n' '{"type":"assistant","message":{"role":"assistant","content":[{"type":"text","text":"Checking"}]}}' "$1""#, "sh", result]);
            let answer = run(
                cmd,
                AskEngine::Claude,
                "fake-claude",
                Duration::from_secs(5),
                None,
            )
            .await;
            assert_eq!(answer.is_ok(), success);
            if let Ok(answer) = answer {
                assert_eq!(answer.text, "Answer");
            }
        }
    }
}
