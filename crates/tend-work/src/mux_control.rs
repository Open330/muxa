//! Endpoint-aware control for the tmux-compatible hosts. Never send an rmux
//! socket to the native tmux binary, even when both expose the same ids.
use std::cell::RefCell;
use std::path::Path;
use std::process::Command;
use tend::{BackendEndpoint, HostKind};

// Legacy lifecycle helpers share one synchronous transaction. Keep its selected
// endpoint on this worker only; mutating process environment would race other
// MCP requests. The guard also restores it after nested calls or unwinding.
thread_local! {
    static ENDPOINT: RefCell<Option<BackendEndpoint>> = const { RefCell::new(None) };
}

pub fn with_endpoint<T>(endpoint: Option<BackendEndpoint>, operation: impl FnOnce() -> T) -> T {
    struct Restore(Option<BackendEndpoint>);
    impl Drop for Restore {
        fn drop(&mut self) {
            ENDPOINT.with(|slot| {
                slot.replace(self.0.take());
            });
        }
    }
    let _restore = Restore(ENDPOINT.with(|slot| slot.replace(endpoint)));
    operation()
}

fn selected_endpoint() -> Option<BackendEndpoint> {
    ENDPOINT.with(|slot| slot.borrow().clone())
}

pub fn rmux_endpoint() -> Option<String> {
    match selected_endpoint() {
        Some(endpoint) => (endpoint.host == HostKind::Rmux).then_some(endpoint.socket),
        None => tend::backend::rmux::ambient_endpoint(),
    }
}

/// `pane` as the selected server reports it: the rmux endpoint, the one
/// tmux server the selected endpoint names, else every tmux server this
/// process scans (one, under `TEND_TMUX_SOCKET`). Pane ids repeat across
/// servers, so a launch on a pinned server must not read another's `%N`.
pub fn resolve_pane(pane: &str) -> Option<tend::tmux::PaneInfo> {
    use tend::backend::PaneBackend;
    if let Some(rmux) = ambient_rmux() {
        return rmux.resolve_pane(&format!("rmux:{pane}"));
    }
    match selected_endpoint() {
        Some(endpoint) if endpoint.host == HostKind::Tmux => {
            let socket = match tmux_socket_path(&endpoint) {
                Some(path) => path.to_path_buf(),
                None => tend::tmux::resolve_socket_path(&endpoint.socket)?,
            };
            tend::tmux::resolve_pane_at(&socket, pane)
        }
        _ => tend::TmuxBackend::new().resolve_pane(pane),
    }
}

pub fn supported(host: HostKind) -> bool {
    matches!(host, HostKind::Tmux | HostKind::Rmux)
}

/// The rmux server ambient commands go to: the one this shell's `$RMUX` (or
/// the selected endpoint) names, else — on Windows, where rmux is the only
/// tmux-compatible host — rmux's default server, as tmux's default server is
/// everywhere else.
pub fn ambient_rmux() -> Option<tend::RmuxBackend> {
    match rmux_endpoint() {
        Some(socket) => Some(tend::RmuxBackend::with_endpoint(socket)),
        None => (cfg!(windows)
            && selected_endpoint().is_none()
            && tend::tmux::scanner::ambient_mux_allowed())
        .then(tend::RmuxBackend::new),
    }
}

pub fn ambient_command() -> Command {
    if let Some(endpoint) = selected_endpoint() {
        return command(&endpoint).expect("lifecycle endpoint must support mux control");
    }
    if let Some(rmux) = ambient_rmux() {
        rmux.command(None)
    } else {
        tend::tmux::tmux_command_scoped()
    }
}

pub fn ambient_command_with_args<S: AsRef<str>>(args: &[S]) -> anyhow::Result<Command> {
    let args: Vec<_> = args.iter().map(AsRef::as_ref).collect();
    if let Some(rmux) = ambient_rmux() {
        return rmux.control_command(&args).map_err(anyhow::Error::msg);
    }
    let mut command = ambient_command();
    command.args(args);
    Ok(command)
}

/// A tmux endpoint names its server either by the short socket name tendd
/// records on a pane (`default`), which must resolve to a live server, or by
/// the socket's absolute path, which is taken as given — that is the only way
/// to address a server that is not running yet, such as one being brought
/// back after `kill-server`.
fn tmux_socket_path(endpoint: &BackendEndpoint) -> Option<&Path> {
    let path = Path::new(&endpoint.socket);
    path.is_absolute().then_some(path)
}

pub fn command(endpoint: &BackendEndpoint) -> Result<Command, String> {
    match endpoint.host {
        HostKind::Tmux => Ok(match tmux_socket_path(endpoint) {
            Some(path) => {
                let mut command = tend::tmux::tmux_command();
                command.arg("-S").arg(path);
                command
            }
            None => tend::tmux::tmux_command_on(Some(&endpoint.socket)),
        }),
        HostKind::Rmux => Ok(tend::RmuxBackend::with_endpoint(&endpoint.socket).command(None)),
        host => Err(format!("{host} does not support this operation")),
    }
}

pub fn capture(endpoint: &BackendEndpoint, args: &[&str]) -> Result<String, String> {
    crate::subprocess::ensure_time_left().map_err(|error| error.to_string())?;
    match endpoint.host {
        HostKind::Tmux => match tmux_socket_path(endpoint) {
            Some(path) => tend::tmux::capture_control_at(path, args),
            None => tend::tmux::capture_control_on(Some(&endpoint.socket), args),
        }
        .map_err(|error| error.to_string()),
        HostKind::Rmux => tend::RmuxBackend::with_endpoint(&endpoint.socket).capture_control(args),
        host => Err(format!("{host} does not support this operation")),
    }
}

pub fn run(endpoint: &BackendEndpoint, args: &[&str]) -> Result<(), String> {
    capture(endpoint, args).map(|_| ())
}

/// Each command's output, in order, from as few round trips as the server
/// allows. tmux runs them in one client, `;`-separated, with a marker line
/// printed between them; any other host, a command with an argument tmux
/// would read as a separator (a trailing `;`), or a batch that fails runs
/// them one by one instead, so every command still gets its own answer.
///
/// For commands that are safe to run twice (reads, `set-option`): after a
/// failed batch some of them may already have run.
pub fn capture_each(
    endpoint: &BackendEndpoint,
    commands: &[Vec<String>],
) -> Vec<Result<String, String>> {
    if let Some(outputs) = capture_batch(endpoint, commands) {
        return outputs.into_iter().map(Ok).collect();
    }
    commands
        .iter()
        .map(|command| {
            let words: Vec<&str> = command.iter().map(String::as_str).collect();
            capture(endpoint, &words)
        })
        .collect()
}

fn capture_batch(endpoint: &BackendEndpoint, commands: &[Vec<String>]) -> Option<Vec<String>> {
    if endpoint.host != HostKind::Tmux
        || commands.len() < 2
        || commands.iter().flatten().any(|word| word.ends_with(';'))
    {
        return None;
    }
    let marker = batch_marker();
    let mut words: Vec<&str> = Vec::new();
    for command in commands {
        words.extend(command.iter().map(String::as_str));
        words.extend([";", "display-message", "-p", marker.as_str(), ";"]);
    }
    words.pop();
    let out = capture(endpoint, &words).ok()?;
    split_batch(&out, &marker, commands.len())
}

/// A line no command's output carries by accident.
fn batch_marker() -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.subsec_nanos());
    format!(
        "__tend_batch_{}_{nanos}_{}__",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    )
}

/// `count` outputs, each followed by `marker` on a line of its own; `None`
/// when the reply does not have exactly that shape.
fn split_batch(out: &str, marker: &str, count: usize) -> Option<Vec<String>> {
    let separator = format!("{marker}\n");
    let mut pieces: Vec<String> = Vec::with_capacity(count);
    let mut rest = out;
    for _ in 0..count {
        let at = if rest.starts_with(&separator) {
            0
        } else {
            rest.find(&format!("\n{separator}"))? + 1
        };
        pieces.push(rest[..at].to_owned());
        rest = &rest[at + separator.len()..];
    }
    rest.is_empty().then_some(pieces)
}

pub fn peek_popup(client: Option<&str>) -> anyhow::Result<()> {
    let mut words = vec![std::env::current_exe()?.into_os_string()];
    words.extend(std::env::args_os().skip(1).filter(|arg| arg != "--popup"));
    full_popup(&words, client, "100%")
}

pub fn full_popup(
    words: &[std::ffi::OsString],
    client: Option<&str>,
    height: &str,
) -> anyhow::Result<()> {
    let shell_command = popup_shell_command(words)?;
    let mut command = ambient_command();
    command.arg("display-popup");
    if let Some(client) = client.filter(|c| !c.is_empty() && !c.contains("#{")) {
        command.args(["-c", client]);
    }
    let status = command
        .args(["-B", "-E", "-w", "100%", "-h", height, "-x", "0", "-y", "0"])
        .arg(shell_command)
        .status()?;
    anyhow::ensure!(status.success(), "could not open Tend popup: {status}");
    Ok(())
}

pub fn popup_shell_command(words: &[std::ffi::OsString]) -> anyhow::Result<String> {
    let words = words
        .iter()
        .map(|word| {
            word.to_str()
                .ok_or_else(|| anyhow::anyhow!("popup argument is not UTF-8"))
        })
        .collect::<anyhow::Result<Vec<_>>>()?;
    Ok(if cfg!(windows) {
        powershell_command(&words)
    } else {
        posix_command(&words)
    })
}

pub fn posix_command(words: &[&str]) -> String {
    words
        .iter()
        .map(|word| format!("'{}'", word.replace('\'', "'\"'\"'")))
        .collect::<Vec<_>>()
        .join(" ")
}

/// rmux on Windows runs popup commands through its `default-shell`, `pwsh`
/// by default. A line of quoted words is a parse error there — a leading
/// string is an expression, not a command — so the program needs the call
/// operator, and a single quote escapes by doubling.
pub fn powershell_command(words: &[&str]) -> String {
    let quoted = words
        .iter()
        .map(|word| format!("'{}'", word.replace('\'', "''")))
        .collect::<Vec<_>>()
        .join(" ");
    format!("& {quoted}")
}

/// Capture the source pane in the binding, before the popup has a pane of its own.
pub fn agent_move_popup(
    endpoint: &BackendEndpoint,
    client: Option<&str>,
    pane: &str,
    add_pane: bool,
) -> anyhow::Result<()> {
    let mut words = vec![std::env::current_exe()?.into_os_string()];
    words.extend(std::env::args_os().skip(1).filter(|arg| arg != "--popup"));
    if add_pane {
        words.extend(["--pane".into(), pane.into()]);
    }
    let shell_command = popup_shell_command(&words)?;
    let mut command = command(endpoint).map_err(anyhow::Error::msg)?;
    command.arg("display-popup");
    if let Some(client) = client {
        command.args(["-c", client]);
    }
    let status = command
        .args(["-E", "-w", "80%", "-h", "75%"])
        .arg(shell_command)
        .status()?;
    anyhow::ensure!(
        status.success(),
        "could not open agent move popup: {status}"
    );
    Ok(())
}

#[derive(Clone)]
pub enum Control {
    Ambient,
    Endpoint(BackendEndpoint),
}

impl Control {
    pub fn is_rmux(&self) -> bool {
        match self {
            Self::Ambient => ambient_rmux().is_some(),
            Self::Endpoint(endpoint) => endpoint.host == HostKind::Rmux,
        }
    }

    pub fn output(&self, args: &[&str]) -> anyhow::Result<String> {
        crate::subprocess::ensure_time_left()?;
        match self {
            Self::Endpoint(endpoint) => capture(endpoint, args).map_err(anyhow::Error::msg),
            Self::Ambient => {
                if let Some(endpoint) = selected_endpoint() {
                    return capture(&endpoint, args).map_err(anyhow::Error::msg);
                }
                if let Some(rmux) = ambient_rmux() {
                    return rmux.capture_control(args).map_err(anyhow::Error::msg);
                }
                tend::tmux::capture_control_on(None, args).map_err(anyhow::Error::msg)
            }
        }
    }
    pub fn run(&self, args: &[&str]) -> anyhow::Result<()> {
        self.output(args).map(|_| ())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)] // drives a `#!/bin/sh` stand-in for rmux
    #[test]
    fn a_batch_reply_splits_into_one_output_per_command() {
        let reply = "claude\nM\nM\ntwo\nlines\nM\n";
        assert_eq!(
            split_batch(reply, "M", 3),
            Some(vec![
                "claude\n".to_owned(),
                String::new(),
                "two\nlines\n".to_owned()
            ])
        );
        // A marker missing, or output after the last one, is not the batch.
        assert_eq!(split_batch("claude\nM\n", "M", 2), None);
        assert_eq!(split_batch("a\nM\nstray\n", "M", 1), None);
        // The marker only counts on a line of its own.
        assert_eq!(
            split_batch("xM\nM\n", "M", 1),
            Some(vec!["xM\n".to_owned()])
        );
    }

    #[test]
    fn a_batch_is_only_tried_where_tmux_reads_it_as_meant() {
        let tmux = BackendEndpoint {
            host: HostKind::Tmux,
            socket: "/nonexistent/tend-batch-test".into(),
        };
        let rmux = BackendEndpoint {
            host: HostKind::Rmux,
            socket: "/nonexistent/tend-batch-test".into(),
        };
        let two = vec![vec!["a".to_owned()], vec!["b".to_owned()]];
        assert!(capture_batch(&rmux, &two).is_none());
        assert!(capture_batch(&tmux, &two[..1]).is_none());
        let trailing = vec![
            vec!["set".to_owned(), "x;".to_owned()],
            vec!["b".to_owned()],
        ];
        assert!(capture_batch(&tmux, &trailing).is_none());
    }

    #[test]
    fn lifecycle_commands_use_one_rmux_server_and_keep_termination_guards() {
        use std::os::unix::fs::PermissionsExt;
        const PROBE: &str = "TEND_TEST_LIFECYCLE_ROUTING";
        if std::env::var_os(PROBE).is_some() {
            use crate::tmux_work::{ManageAction, ManageRequest};
            let endpoint = BackendEndpoint {
                host: HostKind::Rmux,
                socket: "/tmp/selected-server.sock".into(),
            };
            with_endpoint(Some(endpoint), || {
                let request = |confirm| ManageRequest {
                    action: ManageAction::TerminateAgent,
                    pane: Some("%42".into()),
                    workspace: None,
                    work: None,
                    confirm,
                };
                assert!(crate::tmux_work::manage(request(false))
                    .unwrap_err()
                    .to_string()
                    .contains("confirm=true"));
                let mut unmanaged = request(true);
                unmanaged.pane = Some("%43".into());
                assert!(crate::tmux_work::manage(unmanaged)
                    .unwrap_err()
                    .to_string()
                    .contains("not a tend-managed"));
                crate::tmux_work::manage(request(true)).unwrap();
                let output = ambient_command_with_args(&["split-window", "-t", "%42"])
                    .unwrap()
                    .output()
                    .unwrap();
                assert!(output.status.success());
                assert_eq!(String::from_utf8(output.stdout).unwrap().trim(), "%99");
            });
            let log =
                std::fs::read_to_string(std::env::var("TEND_TEST_ROUTING_LOG").unwrap()).unwrap();
            assert!(
                log.contains("-S /tmp/selected-server.sock kill-pane -t $1:@1.%42"),
                "{log}"
            );
            assert!(
                log.contains("-S /tmp/selected-server.sock split-window -t $1:@1.%42"),
                "{log}"
            );
            assert!(!log.contains("rmux:%"), "{log}");
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let rmux = dir.path().join("rmux");
        std::fs::write(
            &rmux,
            r#"#!/bin/sh
if [ "$1" = "-V" ]; then exit 0; fi
printf '%s\n' "$*" >> "$TEND_TEST_ROUTING_LOG"
shift 2
case "$1" in
list-panes) printf '$1\t@1\t%%42\n';;
display-message) case "$*" in *%43*) printf '0\t\n';; *) printf '1\tclaude\n';; esac;;
split-window) printf '%%99\n';;
esac
"#,
        )
        .unwrap();
        std::fs::set_permissions(&rmux, std::fs::Permissions::from_mode(0o755)).unwrap();
        let output = Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "mux_control::tests::lifecycle_commands_use_one_rmux_server_and_keep_termination_guards", "--nocapture"])
            .env(PROBE, "1").env("PATH", dir.path())
            .env("TEND_TEST_ROUTING_LOG", dir.path().join("commands.log"))
            .env_remove("RMUX").env_remove("RMUX_PANE").env_remove("TMUX").env_remove("TMUX_PANE")
            .output().unwrap();
        assert!(
            output.status.success(),
            "{}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }

    #[test]
    fn lifecycle_scope_is_thread_local_nested_and_restored_after_unwind() {
        let selected = BackendEndpoint {
            host: HostKind::Rmux,
            socket: "/tmp/selected-rmux.sock".into(),
        };
        let before = selected_endpoint();
        with_endpoint(Some(selected.clone()), || {
            assert_eq!(selected_endpoint(), Some(selected.clone()));
            assert_eq!(rmux_endpoint().as_deref(), Some("/tmp/selected-rmux.sock"));
            let args: Vec<_> = ambient_command()
                .get_args()
                .map(|a| a.to_string_lossy().into_owned())
                .collect();
            assert!(
                args.iter().any(|a| a == "/tmp/selected-rmux.sock"),
                "{args:?}"
            );
            std::thread::spawn(|| assert!(selected_endpoint().is_none()))
                .join()
                .unwrap();
            let _ = std::panic::catch_unwind(|| with_endpoint(None, || panic!("rollback")));
            assert_eq!(selected_endpoint(), Some(selected));
        });
        assert_eq!(selected_endpoint(), before);
    }

    #[test]
    fn popup_arguments_survive_shell_metacharacters() {
        let quoted = posix_command(&["a path/tend", "it's $HOME; $(false)", ""]);
        let output = Command::new("/bin/sh")
            .args(["-c", &format!("printf '%s\\0' {quoted}")])
            .output()
            .unwrap();
        assert!(output.status.success());
        assert_eq!(output.stdout, b"a path/tend\0it's $HOME; $(false)\0\0");
    }

    #[test]
    fn powershell_popups_call_the_program_and_double_single_quotes() {
        assert_eq!(
            powershell_command(&[r"C:\Program Files\tend.exe", "watch", "it's $HOME", "%4"]),
            r"& 'C:\Program Files\tend.exe' 'watch' 'it''s $HOME' '%4'"
        );
    }

    #[test]
    fn explicit_endpoints_route_independently_of_the_invoking_terminal() {
        for host in [HostKind::Tmux, HostKind::Rmux] {
            let endpoint = BackendEndpoint {
                host,
                socket: "/tmp/selected-server.sock".into(),
            };
            let cmd = command(&endpoint).unwrap();
            // An absolute path is the caller naming a server, running or
            // not; only a short name has to resolve to a live one.
            assert_eq!(
                cmd.get_args().collect::<Vec<_>>(),
                ["-S", "/tmp/selected-server.sock"]
            );
            let program = std::path::Path::new(cmd.get_program())
                .file_name()
                .unwrap()
                .to_string_lossy();
            assert_eq!(program.contains("rmux"), host == HostKind::Rmux);
        }
        let by_name = command(&BackendEndpoint {
            host: HostKind::Tmux,
            socket: "__tend_no_such_server__".into(),
        })
        .unwrap();
        assert_eq!(by_name.get_args().collect::<Vec<_>>(), ["-S", "/dev/null"]);
        assert!(command(&BackendEndpoint {
            host: HostKind::Zellij,
            socket: String::new()
        })
        .is_err());
    }
}
