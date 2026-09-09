//! Endpoint-aware control for the tmux-compatible hosts. Never send an rmux
//! socket to the native tmux binary, even when both expose the same ids.
use muxa::{BackendEndpoint, HostKind};
use std::process::Command;

pub fn supported(host: HostKind) -> bool {
    matches!(host, HostKind::Tmux | HostKind::Rmux)
}

pub fn ambient_command() -> Command {
    if let Some(socket) = muxa::backend::rmux::endpoint_from_env() {
        muxa::RmuxBackend::with_endpoint(socket).command(None)
    } else {
        muxa::tmux::tmux_command_scoped()
    }
}

pub fn ambient_command_with_args<S: AsRef<str>>(args: &[S]) -> anyhow::Result<Command> {
    let args: Vec<_> = args.iter().map(AsRef::as_ref).collect();
    if let Some(socket) = muxa::backend::rmux::endpoint_from_env() {
        return muxa::RmuxBackend::with_endpoint(socket)
            .control_command(&args)
            .map_err(anyhow::Error::msg);
    }
    let mut command = ambient_command();
    command.args(args);
    Ok(command)
}

pub fn command(endpoint: &BackendEndpoint) -> Result<Command, String> {
    match endpoint.host {
        HostKind::Tmux => Ok(muxa::tmux::tmux_command_on(Some(&endpoint.socket))),
        HostKind::Rmux => Ok(muxa::RmuxBackend::with_endpoint(&endpoint.socket).command(None)),
        host => Err(format!("{host} does not support this operation")),
    }
}

pub fn capture(endpoint: &BackendEndpoint, args: &[&str]) -> Result<String, String> {
    match endpoint.host {
        HostKind::Tmux => muxa::tmux::capture_control_on(Some(&endpoint.socket), args)
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
pub fn watch_popup(client: Option<&str>) -> anyhow::Result<()> {
    let executable = std::env::current_exe()?;
    let mut words = vec![executable.into_os_string()];
    words.extend(std::env::args_os().skip(1).filter(|arg| arg != "--popup"));
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
    words
        .iter()
        .map(|word| {
            let word = word
                .to_str()
                .ok_or_else(|| anyhow::anyhow!("popup argument is not UTF-8"))?;
            Ok(format!("'{}'", word.replace('\'', "'\"'\"'")))
        })
        .collect::<anyhow::Result<Vec<_>>>()
        .map(|words| words.join(" "))
}

#[derive(Clone)]
pub enum Control {
    Ambient,
    Endpoint(BackendEndpoint),
}

impl Control {
    pub fn is_rmux(&self) -> bool {
        match self {
            Self::Ambient => muxa::backend::rmux::endpoint_from_env().is_some(),
            Self::Endpoint(endpoint) => endpoint.host == HostKind::Rmux,
        }
    }

    pub fn output(&self, args: &[&str]) -> anyhow::Result<String> {
        match self {
            Self::Endpoint(endpoint) => capture(endpoint, args).map_err(anyhow::Error::msg),
            Self::Ambient => {
                if let Some(socket) = muxa::backend::rmux::endpoint_from_env() {
                    return muxa::RmuxBackend::with_endpoint(socket)
                        .capture_control(args)
                        .map_err(anyhow::Error::msg);
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
    fn popup_arguments_survive_shell_metacharacters() {
        let words: Vec<std::ffi::OsString> = ["a path/muxa", "it's $HOME; $(false)", ""]
            .into_iter()
            .map(Into::into)
            .collect();
        let quoted = popup_shell_command(&words).unwrap();
        let output = Command::new("/bin/sh")
            .args(["-c", &format!("printf '%s\\0' {quoted}")])
            .output()
            .unwrap();
        assert!(output.status.success());
        assert_eq!(output.stdout, b"a path/muxa\0it's $HOME; $(false)\0\0");
    }

    #[test]
    fn explicit_endpoints_route_independently_of_the_invoking_terminal() {
        for host in [HostKind::Tmux, HostKind::Rmux] {
            let endpoint = BackendEndpoint {
                host,
                socket: "/tmp/selected-server.sock".into(),
            };
            let cmd = command(&endpoint).unwrap();
            let expected_socket = if host == HostKind::Tmux {
                "/dev/null"
            } else {
                "/tmp/selected-server.sock"
            };
            assert_eq!(cmd.get_args().collect::<Vec<_>>(), ["-S", expected_socket]);
            let program = std::path::Path::new(cmd.get_program())
                .file_name()
                .unwrap()
                .to_string_lossy();
            assert_eq!(program.contains("rmux"), host == HostKind::Rmux);
        }
        assert!(command(&BackendEndpoint {
            host: HostKind::Zellij,
            socket: String::new()
        })
        .is_err());
    }
}
