//! Reading and replacing `config.toml` as a whole document.
//!
//! Muxa.app's Advanced settings edits the daemon's configuration file
//! directly, so the daemon has to hand out its current text and take a
//! replacement back. Every write is checked before it lands: the document
//! must parse as a [`Config`] and pass [`Config::validate`], and the caller
//! may pin the text it edited so two editors cannot silently overwrite each
//! other. The file is replaced through a temporary file in the same
//! directory, keeping the mode of the file it replaces.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::config::Config;

pub(crate) static CONFIG_WRITE_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LaunchProvider {
    pub program: String,
    pub options: Option<Vec<String>>,
    pub effective_options: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LaunchPipelineAgent {
    pub pipeline: String,
    pub index: usize,
    pub name: String,
    pub program: String,
    pub options: Option<Vec<String>>,
    pub effective_options: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LaunchLegacyGuide {
    pub program: Option<String>,
    pub options: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LaunchSettings {
    pub providers: Vec<LaunchProvider>,
    pub pipelines: Vec<LaunchPipelineAgent>,
    pub legacy_guide: LaunchLegacyGuide,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LaunchDocument {
    pub config: ConfigDocument,
    pub launch: LaunchSettings,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "target", rename_all = "snake_case", deny_unknown_fields)]
pub enum LaunchEdit {
    Provider {
        program: String,
        #[serde(deserialize_with = "deserialize_launch_options")]
        options: Option<Vec<String>>,
    },
    Pipeline {
        pipeline: String,
        index: usize,
        #[serde(deserialize_with = "deserialize_launch_options")]
        options: Option<Vec<String>>,
    },
}

fn deserialize_launch_options<'de, Decoder: serde::Deserializer<'de>>(
    decoder: Decoder,
) -> Result<Option<Vec<String>>, Decoder::Error> {
    Option::<Vec<String>>::deserialize(decoder)
}

fn launch_settings(text: &str) -> Result<LaunchSettings, ConfigFileError> {
    let config: Config =
        toml::from_str(text).map_err(|error| ConfigFileError::Invalid(error.to_string()))?;
    config
        .validate()
        .map_err(|error| ConfigFileError::Invalid(error.to_string()))?;
    let raw: toml::Value =
        toml::from_str(text).map_err(|error| ConfigFileError::Invalid(error.to_string()))?;
    let providers = ["claude", "codex", "gemini", "opencode", "agy"]
        .into_iter()
        .map(|program| LaunchProvider {
            program: program.to_owned(),
            options: config.agent.get(program).map(|entry| entry.options.clone()),
            effective_options: config.launch_options(program, None),
        })
        .collect();
    let mut pipelines = Vec::new();
    for (pipeline, entry) in &config.pipeline {
        for (index, agent) in entry.agent.iter().enumerate() {
            let options = raw
                .get("pipeline")
                .and_then(|value| value.get(pipeline))
                .and_then(|value| value.get("agent"))
                .and_then(|value| value.get(index))
                .and_then(|value| value.get("options"))
                .map(|value| value.clone().try_into::<Vec<String>>())
                .transpose()
                .map_err(|error| ConfigFileError::Invalid(error.to_string()))?;
            pipelines.push(LaunchPipelineAgent {
                pipeline: pipeline.clone(),
                index,
                name: agent.alias.clone(),
                program: agent.program.clone(),
                effective_options: config.launch_options(&agent.program, options.as_deref()),
                options,
            });
        }
    }
    Ok(LaunchSettings {
        providers,
        pipelines,
        legacy_guide: LaunchLegacyGuide {
            program: config.mcp.guide.agent,
            options: config.mcp.guide.options,
        },
    })
}

pub fn read_launch(path: &Path) -> Result<LaunchDocument, ConfigFileError> {
    let config = read(path)?;
    let launch = launch_settings(&config.text)?;
    Ok(LaunchDocument { config, launch })
}

pub fn write_launch(
    path: &Path,
    expected_text: &str,
    edits: &[LaunchEdit],
) -> Result<LaunchDocument, ConfigFileError> {
    let _guard = CONFIG_WRITE_LOCK
        .lock()
        .map_err(|error| ConfigFileError::Io(error.to_string()))?;
    let current = read(path)?;
    if current.text != expected_text {
        return Err(ConfigFileError::Conflict {
            current: current.text,
        });
    }
    let mut document = current
        .text
        .parse::<toml_edit::DocumentMut>()
        .map_err(|error| ConfigFileError::Invalid(error.to_string()))?;
    for edit in edits {
        let options = match edit {
            LaunchEdit::Provider { options, .. } | LaunchEdit::Pipeline { options, .. } => options,
        };
        if options
            .as_ref()
            .is_some_and(|options| options.iter().any(|option| option.contains('\0')))
        {
            return Err(ConfigFileError::Invalid(
                "launch options must not contain a NUL byte".into(),
            ));
        }
        match edit {
            LaunchEdit::Provider { program, options } => {
                if !["claude", "codex", "gemini", "opencode", "agy"].contains(&program.as_str()) {
                    return Err(ConfigFileError::Invalid(format!(
                        "unknown provider {program:?}"
                    )));
                }
                if document.get("agent").is_none() && options.is_none() {
                    continue;
                }
                let agents = launch_table(document.as_table_mut(), "agent")?;
                if options.is_none() {
                    agents.remove(program);
                } else {
                    let provider = launch_table(agents, program)?;
                    set_launch_options(provider, options.as_deref());
                }
            }
            LaunchEdit::Pipeline {
                pipeline,
                index,
                options,
            } => {
                let agents = document
                    .get_mut("pipeline")
                    .and_then(|item| item.get_mut(pipeline))
                    .and_then(|item| item.get_mut("agent"))
                    .ok_or_else(|| {
                        ConfigFileError::Invalid(format!("pipeline {pipeline:?} no longer exists"))
                    })?;
                let table: Option<&mut dyn toml_edit::TableLike> = match agents {
                    toml_edit::Item::ArrayOfTables(tables) => tables
                        .get_mut(*index)
                        .map(|table| table as &mut dyn toml_edit::TableLike),
                    toml_edit::Item::Value(toml_edit::Value::Array(array)) => array
                        .get_mut(*index)
                        .and_then(toml_edit::Value::as_inline_table_mut)
                        .map(|table| table as &mut dyn toml_edit::TableLike),
                    _ => None,
                };
                let table = table.ok_or_else(|| {
                    ConfigFileError::Invalid(format!(
                        "pipeline {pipeline:?} agent {index} no longer exists"
                    ))
                })?;
                set_launch_options(table, options.as_deref());
            }
        }
    }
    let text = document.to_string();
    let launch = launch_settings(&text)?;
    let config = write_unlocked(path, &text, Some(expected_text))?;
    Ok(LaunchDocument { config, launch })
}

fn launch_table<'table>(
    parent: &'table mut dyn toml_edit::TableLike,
    key: &str,
) -> Result<&'table mut dyn toml_edit::TableLike, ConfigFileError> {
    if !parent.contains_key(key) {
        let mut table = toml_edit::Table::new();
        table.set_implicit(true);
        parent.insert(key, toml_edit::Item::Table(table));
    }
    parent
        .get_mut(key)
        .and_then(toml_edit::Item::as_table_like_mut)
        .ok_or_else(|| ConfigFileError::Invalid(format!("{key:?} is not a table")))
}

fn set_launch_options(table: &mut dyn toml_edit::TableLike, options: Option<&[String]>) {
    if let Some(options) = options {
        let array: toml_edit::Array = options.iter().map(String::as_str).collect();
        let mut value = toml_edit::Value::Array(array);
        if let Some(old) = table.get("options").and_then(toml_edit::Item::as_value) {
            *value.decor_mut() = old.decor().clone();
        }
        table.insert("options", toml_edit::Item::Value(value));
    } else {
        table.remove("options");
    }
}

/// The configuration file as a client sees it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConfigDocument {
    pub path: PathBuf,
    /// The file's contents; empty when it does not exist yet.
    pub text: String,
    pub exists: bool,
}

/// What went wrong, in the words a client should show verbatim.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConfigFileError {
    /// The file changed since the caller read it.
    Conflict {
        /// The text on disk now, so the caller can merge instead of asking
        /// for it again.
        current: String,
    },
    /// The replacement does not parse, or fails a semantic check.
    Invalid(String),
    /// The file could not be read or written.
    Io(String),
}

impl std::fmt::Display for ConfigFileError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Conflict { .. } => write!(
                f,
                "config.toml changed on disk since it was read; reload it and apply the edit again"
            ),
            Self::Invalid(detail) | Self::Io(detail) => write!(f, "{detail}"),
        }
    }
}

impl std::error::Error for ConfigFileError {}

/// Reads the configuration file. A missing file is not an error: the daemon
/// runs on defaults, and the editor should offer to create one.
pub fn read(path: &Path) -> Result<ConfigDocument, ConfigFileError> {
    match std::fs::read_to_string(path) {
        Ok(text) => Ok(ConfigDocument {
            path: path.to_path_buf(),
            text,
            exists: true,
        }),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(ConfigDocument {
            path: path.to_path_buf(),
            text: String::new(),
            exists: false,
        }),
        Err(error) => Err(ConfigFileError::Io(format!(
            "reading {}: {error}",
            path.display()
        ))),
    }
}

/// Replaces the configuration file with `text`.
///
/// `expected` is the text the caller started from; when it is given and no
/// longer matches what is on disk, the write is refused and the current text
/// comes back with the error. Nothing is written unless the replacement
/// parses as a [`Config`] and passes [`Config::validate`], so a daemon
/// restart can never find a file it cannot load.
pub fn write(
    path: &Path,
    text: &str,
    expected: Option<&str>,
) -> Result<ConfigDocument, ConfigFileError> {
    let _guard = CONFIG_WRITE_LOCK
        .lock()
        .map_err(|error| ConfigFileError::Io(error.to_string()))?;
    write_unlocked(path, text, expected)
}

fn write_unlocked(
    path: &Path,
    text: &str,
    expected: Option<&str>,
) -> Result<ConfigDocument, ConfigFileError> {
    let current = read(path)?;
    if let Some(expected) = expected {
        if expected != current.text {
            return Err(ConfigFileError::Conflict {
                current: current.text,
            });
        }
    }

    let parsed: Config = toml::from_str(text).map_err(|error| {
        ConfigFileError::Invalid(format!(
            "the config would not parse, so it was not written: {error}"
        ))
    })?;
    parsed.validate().map_err(|error| {
        ConfigFileError::Invalid(format!(
            "the config is invalid, so it was not written: {error}"
        ))
    })?;

    atomic_write(path, text).map_err(ConfigFileError::Io)?;
    Ok(ConfigDocument {
        path: path.to_path_buf(),
        text: text.to_string(),
        exists: true,
    })
}

/// Write-then-rename in the target's directory, keeping the mode of the file
/// being replaced. A fresh file is owner-only, like the one `muxa init`
/// writes.
fn atomic_write(path: &Path, text: &str) -> Result<(), String> {
    use std::io::Write as _;

    if let Some(parent) = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
    {
        std::fs::create_dir_all(parent)
            .map_err(|error| format!("creating {}: {error}", parent.display()))?;
    }
    let permissions = match std::fs::metadata(path) {
        Ok(metadata) => Some(metadata.permissions()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(error) => {
            return Err(format!("reading mode of {}: {error}", path.display()));
        }
    };
    let tmp = path.with_extension(format!("toml.{}.config.tmp", std::process::id()));
    let result = (|| -> std::io::Result<()> {
        let mut file = std::fs::File::create(&tmp)?;
        file.write_all(text.as_bytes())?;
        match permissions {
            Some(permissions) => file.set_permissions(permissions)?,
            #[cfg(unix)]
            None => {
                use std::os::unix::fs::PermissionsExt as _;
                file.set_permissions(std::fs::Permissions::from_mode(0o600))?;
            }
            #[cfg(not(unix))]
            None => {}
        }
        file.sync_all()?;
        drop(file);
        std::fs::rename(&tmp, path)
    })();
    if let Err(error) = result {
        let _ = std::fs::remove_file(&tmp);
        return Err(format!("writing {}: {error}", path.display()));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn launch_reads_presence_and_replacement_precedence() {
        let settings = launch_settings(
            r#"
[mcp.guide]
agent = "codex"
options = ["--model", "legacy"]
[agent.claude]
options = []
[[pipeline."with.dot".agent]]
alias = "inherits"
program = "codex"
[[pipeline."with.dot".agent]]
alias = "empty"
program = "codex"
options = []
[[pipeline."with.dot".agent]]
alias = "overrides"
program = "codex"
options = ["--model", "other"]
"#,
        )
        .unwrap();
        let codex = settings
            .providers
            .iter()
            .find(|entry| entry.program == "codex")
            .unwrap();
        assert_eq!(codex.options, None);
        assert_eq!(codex.effective_options, ["--model", "legacy"]);
        assert_eq!(settings.pipelines[0].options, None);
        assert_eq!(
            settings.pipelines[0].effective_options,
            ["--model", "legacy"]
        );
        assert_eq!(settings.pipelines[1].options, Some(vec![]));
        assert!(settings.pipelines[1].effective_options.is_empty());
        assert_eq!(
            settings.pipelines[2].effective_options,
            ["--model", "other"]
        );
    }

    #[test]
    fn launch_wire_requires_explicit_options_and_rejects_unknown_fields() {
        assert!(
            serde_json::from_str::<LaunchEdit>(r#"{"target":"provider","program":"codex"}"#)
                .is_err()
        );
        assert!(serde_json::from_str::<LaunchEdit>(
            r#"{"target":"provider","program":"codex","options":null,"typo":true}"#
        )
        .is_err());
        let inherited: LaunchEdit =
            serde_json::from_str(r#"{"target":"provider","program":"codex","options":null}"#)
                .unwrap();
        assert_eq!(
            inherited,
            LaunchEdit::Provider {
                program: "codex".into(),
                options: None
            }
        );
        let empty: LaunchEdit = serde_json::from_str(
            r#"{"target":"pipeline","pipeline":"demo","index":0,"options":[]}"#,
        )
        .unwrap();
        assert_eq!(
            empty,
            LaunchEdit::Pipeline {
                pipeline: "demo".into(),
                index: 0,
                options: Some(vec![])
            }
        );
    }

    #[test]
    fn launch_patch_preserves_legacy_comments_and_literal_arguments() {
        let dir = temp_dir("launch-patch");
        let path = dir.join("config.toml");
        let source = "# keep header\n[mcp.guide]\nagent = 'codex'\noptions = ['legacy'] # keep legacy\n\n[[pipeline.'with.dot'.agent]]\nalias = 'review'\nprogram = 'codex'\nprompt = '''\noptions = ['not an option']\n'''\noptions = [\n  'old',\n] # keep suffix\n";
        std::fs::write(&path, source).unwrap();
        let arguments = vec![
            "--model".into(),
            "a b\"c\\d\n한글".into(),
            String::new(),
            "#[]".into(),
        ];
        let saved = write_launch(
            &path,
            source,
            &[
                LaunchEdit::Provider {
                    program: "codex".into(),
                    options: Some(vec![]),
                },
                LaunchEdit::Pipeline {
                    pipeline: "with.dot".into(),
                    index: 0,
                    options: Some(arguments.clone()),
                },
            ],
        )
        .unwrap();
        assert!(saved.config.text.contains("# keep header"));
        assert!(saved
            .config
            .text
            .contains("options = ['legacy'] # keep legacy"));
        assert!(saved.config.text.contains("# keep suffix"));
        assert!(saved.config.text.contains("options = ['not an option']"));
        assert_eq!(saved.launch.pipelines[0].options, Some(arguments));
        let inherited = write_launch(
            &path,
            &saved.config.text,
            &[
                LaunchEdit::Provider {
                    program: "codex".into(),
                    options: None,
                },
                LaunchEdit::Pipeline {
                    pipeline: "with.dot".into(),
                    index: 0,
                    options: None,
                },
            ],
        )
        .unwrap();
        assert_eq!(inherited.launch.pipelines[0].options, None);
        assert_eq!(inherited.launch.pipelines[0].effective_options, ["legacy"]);
        assert_eq!(inherited.launch.legacy_guide.options, ["legacy"]);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn launch_patch_handles_inline_and_dotted_tables() {
        for source in [
            "agent = { codex = { options = ['old'] } }\npipeline = { demo = { agent = [{alias='review',program='codex'}] } }\n",
            "agent.codex.options = ['old']\npipeline.demo.agent = [{alias='review',program='codex'}]\n",
        ] {
            let dir = temp_dir("launch-inline");
            let path = dir.join("config.toml");
            std::fs::write(&path, source).unwrap();
            let saved = write_launch(
                &path,
                source,
                &[
                    LaunchEdit::Provider {
                        program: "codex".into(),
                        options: Some(vec!["new".into()]),
                    },
                    LaunchEdit::Pipeline {
                        pipeline: "demo".into(),
                        index: 0,
                        options: Some(vec![]),
                    },
                ],
            )
            .unwrap();
            assert_eq!(saved.launch.pipelines[0].options, Some(vec![]));
            assert_eq!(
                saved
                    .launch
                    .providers
                    .iter()
                    .find(|entry| entry.program == "codex")
                    .unwrap()
                    .options,
                Some(vec!["new".into()])
            );
            std::fs::remove_dir_all(dir).unwrap();
        }
    }

    #[test]
    fn launch_invalid_batch_and_stale_indices_never_write() {
        let dir = temp_dir("launch-refuse");
        let path = dir.join("config.toml");
        std::fs::write(&path, "# original\n").unwrap();
        for edit in [
            LaunchEdit::Provider {
                program: "unknown".into(),
                options: Some(vec![]),
            },
            LaunchEdit::Provider {
                program: "codex".into(),
                options: Some(vec!["\0".into()]),
            },
            LaunchEdit::Pipeline {
                pipeline: "missing".into(),
                index: 0,
                options: None,
            },
        ] {
            assert!(matches!(
                write_launch(
                    &path,
                    "# original\n",
                    &[
                        LaunchEdit::Provider {
                            program: "claude".into(),
                            options: Some(vec![])
                        },
                        edit,
                    ]
                ),
                Err(ConfigFileError::Invalid(_))
            ));
            assert_eq!(std::fs::read_to_string(&path).unwrap(), "# original\n");
        }
        assert!(matches!(
            write_launch(&path, "", &[]),
            Err(ConfigFileError::Conflict { .. })
        ));
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn launch_concurrent_writers_have_only_one_winner() {
        let dir = temp_dir("launch-race");
        let path = dir.join("config.toml");
        let barrier = std::sync::Arc::new(std::sync::Barrier::new(2));
        let handles: Vec<_> = ["codex", "claude"]
            .into_iter()
            .map(|program| {
                let path = path.clone();
                let barrier = barrier.clone();
                std::thread::spawn(move || {
                    barrier.wait();
                    write_launch(
                        &path,
                        "",
                        &[LaunchEdit::Provider {
                            program: program.into(),
                            options: Some(vec![]),
                        }],
                    )
                })
            })
            .collect();
        let results: Vec<_> = handles
            .into_iter()
            .map(|handle| handle.join().unwrap())
            .collect();
        assert_eq!(results.iter().filter(|result| result.is_ok()).count(), 1);
        assert_eq!(
            results
                .iter()
                .filter(|result| matches!(result, Err(ConfigFileError::Conflict { .. })))
                .count(),
            1
        );
        std::fs::remove_dir_all(dir).unwrap();
    }

    /// A directory of this test's own. Tests run on threads of one process,
    /// so a clock-based name can collide and one test's cleanup then deletes
    /// another's parent mid-write; the counter is what keeps them apart.
    fn temp_dir(label: &str) -> PathBuf {
        use std::sync::atomic::{AtomicU64, Ordering};
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let dir = std::env::temp_dir().join(format!(
            "muxa-config-file-{}-{label}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn reading_a_missing_file_reports_it_without_failing() {
        let dir = temp_dir("missing");
        let path = dir.join("config.toml");

        let document = read(&path).expect("read");

        assert!(!document.exists);
        assert!(document.text.is_empty());
        assert_eq!(document.path, path);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_valid_document_replaces_the_file() {
        let dir = temp_dir("valid");
        let path = dir.join("config.toml");
        std::fs::write(&path, "[ui]\n").unwrap();

        let written = write(&path, "[ask]\nenabled = true\n", Some("[ui]\n")).expect("write");

        assert!(written.exists);
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "[ask]\nenabled = true\n"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_document_that_does_not_parse_leaves_the_file_alone() {
        let dir = temp_dir("unparsed");
        let path = dir.join("config.toml");
        std::fs::write(&path, "[ask]\nenabled = true\n").unwrap();

        let error = write(&path, "[ask\nenabled = true\n", None).expect_err("refused");

        assert!(matches!(error, ConfigFileError::Invalid(_)), "{error:?}");
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "[ask]\nenabled = true\n"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn an_unknown_key_is_refused_before_the_write() {
        let dir = temp_dir("unknown-key");
        let path = dir.join("config.toml");
        std::fs::write(&path, "[ask]\nenabled = true\n").unwrap();

        let error = write(&path, "[ask]\nnot_a_key = 1\n", None).expect_err("refused");

        assert!(matches!(error, ConfigFileError::Invalid(_)), "{error:?}");
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "[ask]\nenabled = true\n"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_stale_editor_is_refused_and_handed_the_current_text() {
        let dir = temp_dir("stale");
        let path = dir.join("config.toml");
        std::fs::write(&path, "[ask]\nenabled = true\n").unwrap();

        let error = write(&path, "[ui]\n", Some("[something else]\n")).expect_err("conflict");

        match error {
            ConfigFileError::Conflict { current } => {
                assert_eq!(current, "[ask]\nenabled = true\n");
            }
            other => panic!("expected a conflict, got {other:?}"),
        }
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "[ask]\nenabled = true\n"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_missing_file_can_be_created_by_pinning_the_empty_text() {
        let dir = temp_dir("create");
        let path = dir.join("nested").join("config.toml");

        let written = write(&path, "[ask]\nenabled = true\n", Some("")).expect("write");

        assert!(written.exists);
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "[ask]\nenabled = true\n"
        );
        std::fs::remove_dir_all(dir).ok();
    }

    #[cfg(unix)]
    #[test]
    fn the_mode_of_the_replaced_file_survives() {
        use std::os::unix::fs::PermissionsExt as _;

        let dir = temp_dir("mode");
        let path = dir.join("config.toml");
        std::fs::write(&path, "[ui]\n").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o640)).unwrap();

        write(&path, "[ask]\nenabled = true\n", None).expect("write");

        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o640);
        std::fs::remove_dir_all(&dir).ok();
    }
}
