//! `muxa config` — read and replace the daemon's `config.toml`.
//!
//! Muxa.app's Advanced settings edits the same file through the same daemon
//! requests, so what the GUI can do the terminal can do too. Every write is
//! parsed and validated by the daemon before it lands, and `--expect` pins
//! the text the caller started from so two editors cannot clobber each
//! other. Editing the file by hand still works; the daemon reads config at
//! startup either way.

use std::io::Read as _;
use std::path::PathBuf;

use anyhow::{bail, Context, Result};
use clap::Subcommand;

use muxa::ipc::Client;

#[derive(Debug, clap::Args)]
pub(crate) struct Args {
    #[command(subcommand)]
    action: ConfigCommand,
}

#[derive(Debug, Subcommand)]
enum ConfigCommand {
    /// Print the path of the config file the daemon loaded.
    Path {
        #[arg(long)]
        json: bool,
    },
    /// Print the config file's contents.
    Show {
        #[arg(long)]
        json: bool,
    },
    /// Replace the config file. The daemon refuses a document that would not
    /// load, so the file is never left in a state the daemon cannot read.
    Set {
        /// File to read the new config from; `-` reads standard input.
        #[arg(long, value_name = "PATH")]
        from: PathBuf,
        /// Refuse the write unless the file still holds this text. Pass the
        /// output of `muxa config show` to make the edit conditional.
        #[arg(long, value_name = "PATH")]
        expect: Option<PathBuf>,
        #[arg(long)]
        json: bool,
    },
    /// Check a document without writing it: the daemon parses and validates
    /// it exactly as `set` would.
    Check {
        /// File to check; `-` reads standard input. Defaults to the config
        /// the daemon currently has.
        #[arg(long, value_name = "PATH")]
        from: Option<PathBuf>,
        #[arg(long)]
        json: bool,
    },
}

pub(crate) async fn run(args: Args, socket: PathBuf) -> Result<()> {
    let client = Client::new(socket);
    match args.action {
        ConfigCommand::Path { json } => {
            let document = read(&client).await?;
            if json {
                println!(
                    "{}",
                    serde_json::json!({
                        "path": document.path,
                        "exists": document.exists,
                    })
                );
            } else {
                println!("{}", document.path.display());
                if !document.exists {
                    eprintln!("(no file yet; the daemon is running on defaults)");
                }
            }
        }
        ConfigCommand::Show { json } => {
            let document = read(&client).await?;
            if json {
                println!("{}", serde_json::to_string(&document)?);
            } else {
                print!("{}", document.text);
            }
        }
        ConfigCommand::Set { from, expect, json } => {
            let text = read_source(&from)?;
            let expected = match expect {
                Some(path) => Some(read_source(&path)?),
                None => None,
            };
            let document = client
                .config_write(&text, expected.as_deref())
                .await
                .context("writing config through muxad")?;
            if json {
                println!("{}", serde_json::to_string(&document)?);
            } else {
                println!("wrote {}", document.path.display());
                eprintln!("restart muxad to apply: muxa daemon restart");
            }
        }
        ConfigCommand::Check { from, json } => {
            let text = match from {
                Some(path) => read_source(&path)?,
                None => read(&client).await?.text,
            };
            // The daemon is the only judge of what loads, and it will not
            // write a document it refuses, so a check is a write of the text
            // that is already there — pinned to itself, which makes it a
            // no-op on success.
            match client.config_write(&text, Some(&text)).await {
                Ok(_) => {
                    if json {
                        println!("{}", serde_json::json!({ "ok": true }));
                    } else {
                        println!("the config loads");
                    }
                }
                Err(error) => {
                    let message = error.to_string();
                    if message.contains("changed on disk") {
                        // The text differs from the file, so it was never a
                        // no-op check; ask for it explicitly instead.
                        bail!(
                            "check compares against the daemon's current file; \
                             pass --from to check a different document"
                        );
                    }
                    if json {
                        println!("{}", serde_json::json!({ "ok": false, "error": message }));
                        std::process::exit(1);
                    }
                    bail!(message);
                }
            }
        }
    }
    Ok(())
}

async fn read(client: &Client) -> Result<muxa::config_file::ConfigDocument> {
    client
        .config_read()
        .await
        .context("reading config through muxad")
}

/// Reads a document from a path, or from standard input for `-`.
fn read_source(path: &std::path::Path) -> Result<String> {
    if path == std::path::Path::new("-") {
        let mut text = String::new();
        std::io::stdin()
            .read_to_string(&mut text)
            .context("reading config from stdin")?;
        return Ok(text);
    }
    std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser as _;

    #[derive(Debug, clap::Parser)]
    struct Harness {
        #[command(subcommand)]
        cmd: ConfigCommand,
    }

    #[test]
    fn set_requires_a_source_and_accepts_a_pin() {
        let parsed = Harness::try_parse_from([
            "config",
            "set",
            "--from",
            "-",
            "--expect",
            "/tmp/before.toml",
        ])
        .expect("parses");

        match parsed.cmd {
            ConfigCommand::Set { from, expect, json } => {
                assert_eq!(from, PathBuf::from("-"));
                assert_eq!(expect, Some(PathBuf::from("/tmp/before.toml")));
                assert!(!json);
            }
            other => panic!("expected set, got {other:?}"),
        }

        assert!(Harness::try_parse_from(["config", "set"]).is_err());
    }

    #[test]
    fn check_defaults_to_the_daemons_own_document() {
        let parsed = Harness::try_parse_from(["config", "check"]).expect("parses");

        match parsed.cmd {
            ConfigCommand::Check { from, json } => {
                assert!(from.is_none());
                assert!(!json);
            }
            other => panic!("expected check, got {other:?}"),
        }
    }

    #[test]
    fn reading_a_source_takes_stdin_for_a_dash() {
        let dir = std::env::temp_dir().join(format!("muxa-config-cmd-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.toml");
        std::fs::write(&path, "[ui]\n").unwrap();

        assert_eq!(read_source(&path).unwrap(), "[ui]\n");
        assert!(read_source(&dir.join("missing.toml")).is_err());

        std::fs::remove_dir_all(&dir).ok();
    }
}
