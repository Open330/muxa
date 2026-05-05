//! `muxa logs` — tail muxad's stdout/stderr without remembering paths.
//!
//! Default sources are `/tmp/muxad.log` and `/tmp/muxad.err` (the paths the
//! macOS launchd plist redirects to, and where the install wizard's
//! `nohup`-fallback sends muxad on Linux). On Linux hosts that ran
//! `muxa init` with the systemd user unit, the unit doesn't redirect —
//! logs go to journald — so we transparently fall back to
//! `journalctl --user -u muxad` when `/tmp/muxad.log` is missing and
//! `journalctl` is on PATH.
//!
//! UX is deliberately simple: a one-line header naming the sources, then
//! `tail`-like streaming with optional follow. SIGINT exits cleanly.

use anyhow::{Context, Result};
use clap::Parser;
use std::collections::VecDeque;
use std::io::IsTerminal;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncSeekExt, BufReader};

const DEFAULT_LOG: &str = "/tmp/muxad.log";
const DEFAULT_ERR: &str = "/tmp/muxad.err";

/// How often the follow loop wakes to check for newly-appended bytes. 100ms
/// is responsive enough to feel live without burning CPU on idle daemons.
const POLL_INTERVAL: Duration = Duration::from_millis(100);

#[derive(Debug, Parser)]
pub struct Args {
    /// Number of trailing lines to print before following.
    #[arg(short = 'n', long = "lines", default_value_t = 30)]
    lines: usize,

    /// Print the trailing lines and exit instead of following.
    #[arg(short = 'N', long = "no-follow")]
    no_follow: bool,

    /// Only show `/tmp/muxad.err` (skip the stdout log).
    #[arg(long = "err-only")]
    err_only: bool,

    /// Only print lines containing this substring (case-insensitive). Handy
    /// for `--filter ERROR` or `--filter ipc.handle`.
    #[arg(long = "filter")]
    filter: Option<String>,
}

/// Where logs come from for a given invocation. We resolve this once up
/// front so the header can name the exact sources/command.
enum Source {
    /// One or more on-disk files. Empty list means "no log files exist
    /// and journalctl unavailable" — we surface that as a friendly error.
    Files(Vec<PathBuf>),
    /// `journalctl --user -u muxad` (Linux-only fallback). Stored as the
    /// argv we'd actually exec, so the header shows what the user could
    /// run by hand.
    Journalctl { argv: Vec<String> },
}

pub async fn run(args: Args) -> Result<()> {
    let colored = use_colors();
    let filter = args.filter.as_deref().map(str::to_ascii_lowercase);
    let source = resolve_source(args.err_only, args.no_follow, args.lines)?;

    print_header(&source, args.no_follow);

    match source {
        Source::Files(paths) => {
            // Print the trailing N lines of each file in the order given,
            // then (if following) watch all of them concurrently. Reading
            // the head and following are split so a filter doesn't confuse
            // "what was already there" with "what just arrived".
            for path in &paths {
                let lines = tail_n_lines(path, args.lines)
                    .await
                    .with_context(|| format!("reading {}", path.display()))?;
                for line in lines {
                    emit_line(&line, filter.as_deref(), colored);
                }
            }
            if !args.no_follow {
                follow_files(&paths, filter.as_deref(), colored).await?;
            }
        }
        Source::Journalctl { argv } => {
            // journalctl handles its own `-n N` and `-f` semantics — we
            // just stream stdout. The trailing-lines window is already
            // baked into argv by `resolve_source`.
            run_journalctl(&argv, filter.as_deref(), colored).await?;
        }
    }

    Ok(())
}

fn resolve_source(err_only: bool, no_follow: bool, lines: usize) -> Result<Source> {
    let log = PathBuf::from(DEFAULT_LOG);
    let err = PathBuf::from(DEFAULT_ERR);

    let mut files: Vec<PathBuf> = Vec::new();
    if !err_only && log.exists() {
        files.push(log.clone());
    }
    if err.exists() {
        files.push(err);
    }

    if !files.is_empty() {
        return Ok(Source::Files(files));
    }

    // No on-disk logs found. On Linux, the systemd user unit doesn't
    // redirect stdout/stderr, so logs land in journald — try that.
    // `--err-only` doesn't have a clean journalctl analog (priority
    // filtering would drop INFO lines from the same unit), so we still
    // fall through to journalctl but include all priorities.
    if which_journalctl().is_some() {
        let mut argv = vec![
            "journalctl".to_string(),
            "--user".to_string(),
            "-u".to_string(),
            "muxad".to_string(),
            "-n".to_string(),
            lines.to_string(),
        ];
        if !no_follow {
            argv.push("-f".to_string());
        }
        return Ok(Source::Journalctl { argv });
    }

    anyhow::bail!(
        "no muxad logs found: neither {DEFAULT_LOG} nor {DEFAULT_ERR} exist, \
         and `journalctl` is not on PATH. Is muxad running?"
    )
}

fn which_journalctl() -> Option<PathBuf> {
    which::which("journalctl").ok()
}

fn print_header(source: &Source, no_follow: bool) {
    let suffix = if no_follow { "" } else { " (Ctrl-C to exit)" };
    match source {
        Source::Files(paths) => {
            let joined = paths
                .iter()
                .map(|p| p.display().to_string())
                .collect::<Vec<_>>()
                .join(" + ");
            let verb = if no_follow { "showing" } else { "tailing" };
            eprintln!("{verb} {joined}{suffix}");
        }
        Source::Journalctl { argv } => {
            eprintln!("running `{}`{suffix}", argv.join(" "));
        }
    }
}

/// Read the last `n` lines from `path`. Loads the whole file into memory —
/// muxad's logs are small (KB to low-MB) and this keeps the implementation
/// trivial. If logs ever grow large we'd need a reverse-seeking byte reader.
async fn tail_n_lines(path: &Path, n: usize) -> Result<Vec<String>> {
    if n == 0 {
        return Ok(Vec::new());
    }
    let bytes = tokio::fs::read(path).await?;
    let text = String::from_utf8_lossy(&bytes);
    let mut buf: VecDeque<String> = VecDeque::with_capacity(n);
    for line in text.lines() {
        if buf.len() == n {
            buf.pop_front();
        }
        buf.push_back(line.to_string());
    }
    Ok(buf.into())
}

async fn follow_files(paths: &[PathBuf], filter: Option<&str>, colored: bool) -> Result<()> {
    // One follower per file. We seek each to its current end so we don't
    // re-emit the trailing-N window we already printed, then race their
    // next-line reads against ctrl_c.
    let mut readers: Vec<(PathBuf, BufReader<tokio::fs::File>)> = Vec::with_capacity(paths.len());
    for path in paths {
        let mut file = tokio::fs::File::open(path)
            .await
            .with_context(|| format!("opening {} for follow", path.display()))?;
        file.seek(std::io::SeekFrom::End(0)).await?;
        readers.push((path.clone(), BufReader::new(file)));
    }

    let mut ctrl_c = std::pin::pin!(tokio::signal::ctrl_c());
    let mut line_buf = String::new();
    // Snapshot the count once so the inner loop doesn't need to reborrow
    // `readers` while it holds a mutable borrow on each entry.
    let multi = readers.len() > 1;

    loop {
        // Drain whatever's available right now from each reader before
        // sleeping. We can't easily race read_line futures across a Vec
        // (BufReader::read_line wants &mut self), so a poll cycle is
        // simpler and the 100ms cadence is fine for log streams.
        let mut any_progress = false;
        for (path, reader) in &mut readers {
            loop {
                line_buf.clear();
                // Use `read_line` which returns Ok(0) at EOF without
                // blocking once the underlying file's at the end.
                match reader.read_line(&mut line_buf).await {
                    Ok(0) => break,
                    Ok(_) => {
                        any_progress = true;
                        let trimmed = line_buf.trim_end_matches(['\n', '\r']);
                        let prefixed = if multi {
                            format!("[{}] {}", path.display(), trimmed)
                        } else {
                            trimmed.to_string()
                        };
                        emit_line(&prefixed, filter, colored);
                    }
                    Err(e) => {
                        // A transient read error (e.g. logrotate truncated
                        // mid-read) is not fatal — log it to stderr and
                        // keep going. The next poll will pick up wherever
                        // the kernel leaves the position.
                        //
                        // KNOWN LIMITATION: if the file is rotated (moved
                        // and replaced) we keep following the *old* inode.
                        // Re-running `muxa logs` picks up the new file.
                        eprintln!("muxa logs: read error on {}: {e}", path.display());
                        break;
                    }
                }
            }
        }

        if any_progress {
            // Even when progress is made, give ctrl_c a chance to
            // interrupt — otherwise a chatty log could starve the signal
            // handler indefinitely.
            tokio::select! {
                biased;
                _ = ctrl_c.as_mut() => return Ok(()),
                () = std::future::ready(()) => {}
            }
        } else {
            // Race the sleep against ctrl_c so the process exits within
            // one poll interval of the user pressing Ctrl-C.
            tokio::select! {
                () = tokio::time::sleep(POLL_INTERVAL) => {}
                _ = ctrl_c.as_mut() => {
                    return Ok(());
                }
            }
        }
    }
}

async fn run_journalctl(argv: &[String], filter: Option<&str>, colored: bool) -> Result<()> {
    let mut cmd = tokio::process::Command::new(&argv[0]);
    cmd.args(&argv[1..])
        .stdout(Stdio::piped())
        // Inherit stderr so journalctl's own diagnostics surface directly.
        .stderr(Stdio::inherit());

    let mut child = cmd.spawn().with_context(|| {
        format!(
            "failed to spawn `{}` — is journalctl installed?",
            argv.join(" ")
        )
    })?;

    let stdout = child
        .stdout
        .take()
        .context("journalctl child had no stdout pipe")?;
    let mut reader = BufReader::new(stdout).lines();

    let mut ctrl_c = std::pin::pin!(tokio::signal::ctrl_c());
    loop {
        tokio::select! {
            line = reader.next_line() => {
                match line? {
                    Some(text) => emit_line(&text, filter, colored),
                    None => break,
                }
            }
            _ = ctrl_c.as_mut() => {
                // SIGINT also reaches the child (it's in our process
                // group), so journalctl will exit on its own. We just
                // stop relaying.
                break;
            }
        }
    }

    // Best-effort reap so we don't leave a zombie if the child outlived us.
    let _ = child.start_kill();
    let _ = child.wait().await;
    Ok(())
}

/// Print a line if it matches the filter. Accepts already-prefixed lines
/// from the multi-file follower so the prefix is part of the haystack.
fn emit_line(line: &str, filter: Option<&str>, colored: bool) {
    if !line_matches_filter(line, filter) {
        return;
    }
    println!("{}", colorize_line(line, colored));
}

/// Case-insensitive substring match. None filter matches everything.
fn line_matches_filter(line: &str, filter: Option<&str>) -> bool {
    match filter {
        None => true,
        Some(needle) => {
            // `needle` is pre-lowercased by the caller; lowercase the
            // haystack here so each line's check is one allocation.
            line.to_ascii_lowercase().contains(needle)
        }
    }
}

/// Wrap a log line in ANSI color codes by severity. We inspect the raw
/// substring rather than parse `tracing` output — muxad emits a mix of
/// `tracing-subscriber` lines and bare panic prints, so keyword matching
/// is the only common denominator.
fn colorize_line(line: &str, colored: bool) -> String {
    if !colored {
        return line.to_string();
    }
    // ERROR / panic → red (bug-grade); WARN → yellow. Order matters:
    // a "panicked at … WARN something" should be red, not yellow.
    if line.contains("ERROR") || line.contains("panic") {
        format!("\x1b[31m{line}\x1b[0m")
    } else if line.contains("WARN") {
        format!("\x1b[33m{line}\x1b[0m")
    } else {
        line.to_string()
    }
}

/// Whether to emit ANSI color. Mirrors the `muxa status` rule: respect
/// `NO_COLOR`, and only color when stdout is a TTY so piping to `grep`
/// stays clean.
fn use_colors() -> bool {
    if std::env::var_os("NO_COLOR").is_some() {
        return false;
    }
    std::io::stdout().is_terminal()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write as _;

    #[test]
    fn colorize_line_for_error() {
        let colored = colorize_line("foo ERROR bar", true);
        assert!(
            colored.starts_with("\x1b[31m"),
            "expected red prefix, got {colored:?}"
        );
        assert!(colored.ends_with("\x1b[0m"));
        assert!(colored.contains("foo ERROR bar"));

        let plain = colorize_line("foo ERROR bar", false);
        assert_eq!(plain, "foo ERROR bar");
    }

    #[test]
    fn colorize_line_for_warn() {
        let colored = colorize_line("WARN: slow", true);
        assert!(colored.starts_with("\x1b[33m"));
    }

    #[test]
    fn colorize_line_passthrough_for_info() {
        let colored = colorize_line("INFO: hello", true);
        assert_eq!(colored, "INFO: hello");
    }

    #[test]
    fn filter_predicate_case_insensitive() {
        let needle = Some("warn");
        assert!(line_matches_filter("WARN: x", needle));
        assert!(line_matches_filter("downward warning", needle));
        assert!(!line_matches_filter("INFO: nothing", needle));
    }

    #[test]
    fn filter_predicate_none_matches_everything() {
        assert!(line_matches_filter("any line", None));
        assert!(line_matches_filter("", None));
    }

    #[tokio::test]
    async fn tail_n_lines_returns_trailing_n() {
        let mut tmp = tempfile::NamedTempFile::new().unwrap();
        for i in 0..50 {
            writeln!(tmp, "line {i}").unwrap();
        }
        tmp.flush().unwrap();

        let lines = tail_n_lines(tmp.path(), 10).await.unwrap();
        assert_eq!(lines.len(), 10);
        assert_eq!(lines[0], "line 40");
        assert_eq!(lines[9], "line 49");
    }

    #[tokio::test]
    async fn tail_n_lines_handles_short_file() {
        let mut tmp = tempfile::NamedTempFile::new().unwrap();
        writeln!(tmp, "only line").unwrap();
        tmp.flush().unwrap();

        let lines = tail_n_lines(tmp.path(), 10).await.unwrap();
        assert_eq!(lines, vec!["only line".to_string()]);
    }

    #[tokio::test]
    async fn tail_n_lines_zero_returns_empty() {
        let mut tmp = tempfile::NamedTempFile::new().unwrap();
        writeln!(tmp, "ignored").unwrap();
        tmp.flush().unwrap();

        let lines = tail_n_lines(tmp.path(), 0).await.unwrap();
        assert!(lines.is_empty());
    }
}
