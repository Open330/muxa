//! Endpoint-aware control for the tmux-compatible hosts. Never send an rmux
//! socket to the native tmux binary, even when both expose the same ids.
use muxa::{BackendEndpoint, HostKind};
use std::cell::RefCell;
use std::path::Path;
use std::process::Command;

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
        None => muxa::backend::rmux::endpoint_from_env(),
    }
}

pub fn supported(host: HostKind) -> bool {
    matches!(host, HostKind::Tmux | HostKind::Rmux)
}

/// The rmux server ambient commands go to: the one this shell's `$RMUX` (or
/// the selected endpoint) names, else — on Windows, where rmux is the only
/// tmux-compatible host — rmux's default server, as tmux's default server is
/// everywhere else.
pub fn ambient_rmux() -> Option<muxa::RmuxBackend> {
    match rmux_endpoint() {
        Some(socket) => Some(muxa::RmuxBackend::with_endpoint(socket)),
        None => (cfg!(windows) && selected_endpoint().is_none()).then(muxa::RmuxBackend::new),
    }
}

pub fn ambient_command() -> Command {
    if let Some(endpoint) = selected_endpoint() {
        return command(&endpoint).expect("lifecycle endpoint must support mux control");
    }
    if let Some(rmux) = ambient_rmux() {
        rmux.command(None)
    } else {
        muxa::tmux::tmux_command_scoped()
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

/// A tmux endpoint names its server either by the short socket name muxad
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
                let mut command = muxa::tmux::tmux_command();
                command.arg("-S").arg(path);
                command
            }
            None => muxa::tmux::tmux_command_on(Some(&endpoint.socket)),
        }),
        HostKind::Rmux => Ok(muxa::RmuxBackend::with_endpoint(&endpoint.socket).command(None)),
        host => Err(format!("{host} does not support this operation")),
    }
}

pub fn capture(endpoint: &BackendEndpoint, args: &[&str]) -> Result<String, String> {
    match endpoint.host {
        HostKind::Tmux => match tmux_socket_path(endpoint) {
            Some(path) => muxa::tmux::capture_control_at(path, args),
            None => muxa::tmux::capture_control_on(Some(&endpoint.socket), args),
        }
        .map_err(|error| error.to_string()),
        HostKind::Rmux => muxa::RmuxBackend::with_endpoint(&endpoint.socket).capture_control(args),
        host => Err(format!("{host} does not support this operation")),
    }
}

pub fn run(endpoint: &BackendEndpoint, args: &[&str]) -> Result<(), String> {
    capture(endpoint, args).map(|_| ())
}

/// Resolve the host and socket ourselves: an existing rmux server's run-shell
/// PATH can point `tmux` at native tmux rather than the rmux shim.
pub fn watch_popup(client: Option<&str>, source: Option<&str>) -> anyhow::Result<()> {
    let executable = std::env::current_exe()?;
    let mut words = vec![executable.into_os_string()];
    let work_pane = source.and_then(|pane| crate::sidebar::watch_source(&Control::Ambient, pane));
    let mut args = std::env::args_os().skip(1);
    while let Some(arg) = args.next() {
        if arg == "--popup" {
            continue;
        }
        if work_pane.is_some() && arg == "--caller-pane" {
            args.next();
            continue;
        }
        if work_pane.is_some() && arg.to_string_lossy().starts_with("--caller-pane=") {
            continue;
        }
        words.push(arg);
    }
    if let Some(pane) = work_pane {
        words.extend(["--caller-pane".into(), pane.into()]);
    }
    let shell_command = popup_shell_command(&words)?;
    let mut command = ambient_command();
    command.arg("display-popup");
    if let Some(client) = client.filter(|c| !c.is_empty() && !c.contains("#{")) {
        command.args(["-c", client]);
    }
    let status = command
        .args(["-B", "-E", "-w", "100%", "-h", "99%", "-x", "0", "-y", "0"])
        .arg(shell_command)
        .status()?;
    anyhow::ensure!(status.success(), "could not open watch popup: {status}");
    Ok(())
}

fn popup_shell_command(words: &[std::ffi::OsString]) -> anyhow::Result<String> {
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

fn posix_command(words: &[&str]) -> String {
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
fn powershell_command(words: &[&str]) -> String {
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
        match self {
            Self::Endpoint(endpoint) => capture(endpoint, args).map_err(anyhow::Error::msg),
            Self::Ambient => {
                if let Some(endpoint) = selected_endpoint() {
                    return capture(&endpoint, args).map_err(anyhow::Error::msg);
                }
                if let Some(rmux) = ambient_rmux() {
                    return rmux.capture_control(args).map_err(anyhow::Error::msg);
                }
                let output = ambient_command().args(args).output()?;
                anyhow::ensure!(
                    output.status.success(),
                    "{}: {}",
                    args[0],
                    String::from_utf8_lossy(&output.stderr).trim()
                );
                Ok(String::from_utf8(output.stdout)?)
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

    #[test]
    fn lifecycle_commands_use_one_rmux_server_and_keep_termination_guards() {
        use std::os::unix::fs::PermissionsExt;
        const PROBE: &str = "MUXA_TEST_LIFECYCLE_ROUTING";
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
                    .contains("not a muxa-managed"));
                crate::tmux_work::manage(request(true)).unwrap();
                let output = ambient_command_with_args(&["split-window", "-t", "%42"])
                    .unwrap()
                    .output()
                    .unwrap();
                assert!(output.status.success());
                assert_eq!(String::from_utf8(output.stdout).unwrap().trim(), "%99");
            });
            let log =
                std::fs::read_to_string(std::env::var("MUXA_TEST_ROUTING_LOG").unwrap()).unwrap();
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
printf '%s\n' "$*" >> "$MUXA_TEST_ROUTING_LOG"
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
            .env("MUXA_TEST_ROUTING_LOG", dir.path().join("commands.log"))
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
        let quoted = posix_command(&["a path/muxa", "it's $HOME; $(false)", ""]);
        let output = Command::new("/bin/sh")
            .args(["-c", &format!("printf '%s\\0' {quoted}")])
            .output()
            .unwrap();
        assert!(output.status.success());
        assert_eq!(output.stdout, b"a path/muxa\0it's $HOME; $(false)\0\0");
    }

    #[test]
    fn powershell_popups_call_the_program_and_double_single_quotes() {
        assert_eq!(
            powershell_command(&[r"C:\Program Files\muxa.exe", "watch", "it's $HOME", "%4"]),
            r"& 'C:\Program Files\muxa.exe' 'watch' 'it''s $HOME' '%4'"
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
            socket: "__muxa_no_such_server__".into(),
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
