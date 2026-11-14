//! Fleet commands inside tendd. A controller's `work_command` frame names an
//! argv the relay host used to hand its sibling `tend` binary. The two that
//! start agents run here instead, through the same in-process code their
//! local IPC requests use:
//!
//! - `work up …` (`work_up_in_process::run_argv`), unless
//!   `TEND_WORK_UP_CHILD` forces the child or the argv asks for something
//!   only the CLI's wrapper does (a ticket lookup, `--accept`/`--check`,
//!   `--show-prompts`, no `--yes`);
//! - `agent start …` (`agent_start_in_process::launch_argv`; over Fleet it is
//!   always a tmux launch on the default server, with no `tmux_socket` and
//!   no environment), unless `TEND_AGENT_START_CHILD` forces the child.
//!
//! A frame that carries stdin runs the child, which handed it on (a route's
//! `prepare` reads it); in-process commands get a closed stdin.
//!
//! The answer has the child's shape, which the controller already parses:
//! exit code 0 with the JSON document on stdout, or 1 with `Error: ` and the
//! error chain on stderr, as the CLI's `main` printed it
//! (`work_up_in_process::printed`). Everything else on the
//! allowlist (`work options|preset|pipeline|route|dispatch…|fleet-ask`,
//! `swap`) still runs the child: that code is still in the CLI crate.

use std::path::Path;
use tend::work_control::WorkCommandOutput;

/// The in-process answer to an already validated Fleet argv, or `None` to
/// run the child.
pub(crate) async fn run(
    args: &[String],
    stdin: Option<&str>,
    socket: &Path,
) -> Option<WorkCommandOutput> {
    let starts_agents =
        tend::work_control::is_work_up(args) || tend::agent_control::is_agent_start(args);
    if starts_agents && stdin.is_some() {
        // The child got the frame's stdin, and a route's `prepare` read
        // it; in-process commands get a closed stdin.
        tracing::info!("in-process Fleet command declined (stdin); running the tend CLI");
        return None;
    }
    let result = if tend::work_control::is_work_up(args) {
        if tend::work_control::work_up_child_forced() {
            return None;
        }
        crate::work_up_in_process::run_argv(args, socket.to_path_buf()).await?
    } else if tend::agent_control::is_agent_start(args) {
        if tend::agent_control::agent_start_child_forced() {
            return None;
        }
        crate::agent_start_in_process::launch_argv(args, socket.to_path_buf(), None).await?
    } else {
        return None;
    };
    Some(child_output(result))
}

/// What the `tend` child would have exited with and printed.
fn child_output(result: Result<serde_json::Value, String>) -> WorkCommandOutput {
    match result {
        Ok(document) => WorkCommandOutput {
            exit_code: 0,
            stdout: serde_json::to_string_pretty(&document).unwrap_or_default() + "\n",
            stderr: String::new(),
        },
        Err(error) => WorkCommandOutput {
            exit_code: 1,
            stdout: String::new(),
            stderr: format!("Error: {error}\n"),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_answer_reads_as_the_child_printed_it() {
        let ok = child_output(Ok(serde_json::json!({"schema_version": 1})));
        assert_eq!(ok.exit_code, 0);
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&ok.stdout).unwrap(),
            serde_json::json!({"schema_version": 1})
        );
        let failed = child_output(Err("no route\n\nCaused by:\n    bad cwd".into()));
        assert_eq!(failed.exit_code, 1);
        // What the controller reads back: the last stderr line.
        let agent = tend::agent_control::parse_output(&failed, Some("dev")).unwrap_err();
        assert!(agent.to_string().ends_with(" bad cwd"), "{agent}");
        let work = tend::work_control::work_up_result(&failed).unwrap_err();
        assert!(work.to_string().ends_with("bad cwd"), "{work}");
    }

    #[tokio::test]
    async fn a_frame_with_stdin_runs_the_child() {
        let socket = Path::new("/nonexistent/tendd.sock");
        for words in [
            &["work", "up", "ENG-1", "--json", "--yes", "--no-ticket"][..],
            &["agent", "start", "--json", "--agent=codex", "--host=tmux"],
        ] {
            let args: Vec<String> = words.iter().map(|word| (*word).to_string()).collect();
            assert!(
                run(&args, Some("payload"), socket).await.is_none(),
                "{words:?}"
            );
        }
    }

    #[tokio::test]
    async fn everything_else_on_the_allowlist_runs_the_child() {
        let socket = Path::new("/nonexistent/tendd.sock");
        for words in [
            &["work", "options", "--json"][..],
            &["work", "pipeline", "show", "--json"],
            &["swap", "%1", "--json"],
        ] {
            let args: Vec<String> = words.iter().map(|word| (*word).to_string()).collect();
            assert!(run(&args, None, socket).await.is_none(), "{words:?}");
        }
    }
}
