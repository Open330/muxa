//! Disabled starter rules, installed without replacing operator-owned rules.

use std::path::Path;

use crate::automation::{
    AutomationAction, AutomationEvent, AutomationRule, DurationSpec, WaitSpec,
};
use crate::config::Config;

/// Ready-to-enable rules. Names are stable identities; existing names win.
#[must_use]
pub fn rules() -> Vec<AutomationRule> {
    let mut capacity = AutomationRule::new(
        "resume-codex-capacity",
        AutomationEvent::Error,
        AutomationAction::SendPrompt,
    );
    capacity.agent = vec![crate::automation::AgentKindSpec(crate::AgentKind::Codex)];
    capacity.notification = Some(r"(?i)^Selected model is at capacity\.".into());
    capacity.text = Some("continue".into());
    capacity.wait = Some(WaitSpec::after_event(time::Duration::minutes(2)));
    capacity.max_per_hour = Some(6);

    let mut limit = AutomationRule::new(
        "resume-after-limit",
        AutomationEvent::RateLimited,
        AutomationAction::SendPrompt,
    );
    limit.text = Some("continue".into());
    limit.wait = Some(WaitSpec::after_reset(time::Duration::minutes(2)));
    limit.fallback = Some(DurationSpec::seconds(20 * 60));

    let mut input = AutomationRule::new(
        "notify-waiting-input",
        AutomationEvent::WaitingInput,
        AutomationAction::Notify,
    );
    input.message = Some("Agent has been waiting for input for five minutes.".into());
    input.wait = Some(WaitSpec::after_event(time::Duration::minutes(5)));

    let mut error = AutomationRule::new(
        "notify-persistent-error",
        AutomationEvent::Error,
        AutomationAction::Notify,
    );
    error.message = Some("Agent has remained in an error state for five minutes.".into());
    error.wait = Some(WaitSpec::after_event(time::Duration::minutes(5)));

    vec![capacity, limit, input, error]
        .into_iter()
        .map(|mut rule| {
            rule.enabled = false;
            rule
        })
        .collect()
}

/// Append missing starter rules while retaining comments, custom rules, master
/// switches and all fields of existing same-name rules. `None` means no change.
pub fn merge(original: &str) -> Result<Option<String>, String> {
    let mut document: toml_edit::DocumentMut = original
        .parse()
        .map_err(|error| format!("parsing config for default automations: {error}"))?;
    let tables = crate::automation::automation_rules_mut(&mut document)?;
    let mut changed = false;
    for rule in rules() {
        if tables.iter().any(|table| {
            table.get("name").and_then(toml_edit::Item::as_str) == Some(rule.name.as_str())
        }) {
            continue;
        }
        tables.push(crate::automation::rule_table(&rule)?);
        changed = true;
    }
    if !changed {
        return Ok(None);
    }
    let text = document.to_string();
    let config: Config = toml::from_str(&text).map_err(|error| error.to_string())?;
    config.validate().map_err(|error| error.to_string())?;
    Ok(Some(text))
}

/// Reconcile binary-only upgrades too. Atomic, no write when already installed.
pub fn install(path: &Path) -> Result<bool, String> {
    let _guard = crate::config_file::CONFIG_WRITE_LOCK
        .lock()
        .map_err(|error| error.to_string())?;
    let original = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(error) => return Err(format!("reading {}: {error}", path.display())),
    };
    let Some(text) = merge(&original)? else {
        return Ok(false);
    };
    crate::automation::atomic_write(path, &text)?;
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fresh_install_has_four_valid_disabled_rules_and_is_idempotent() {
        let text = merge("").unwrap().unwrap();
        let config: Config = toml::from_str(&text).unwrap();
        assert_eq!(config.automation.rule.len(), 4);
        assert!(config.automation.rule.iter().all(|rule| !rule.enabled));
        config.validate().unwrap();
        assert_eq!(merge(&text).unwrap(), None);
    }

    #[test]
    fn upgrade_preserves_customized_enabled_rules_and_engine_controls() {
        let original = r#"# user config
[watch]
theme = "classic"
[automation]
enabled = false
paused_until = "2026-10-14T00:00:00Z"
[[automation.rule]]
# keep my settings
name = "resume-codex-capacity"
enabled = true
on = "error"
action = "send_prompt"
text = "keep going"
wait = "10m"
[[automation.rule]]
name = "my-error-alert"
on = "error"
action = "notify"
message = "custom"
"#;
        let before: Config = toml::from_str(original).unwrap();
        let text = merge(original).unwrap().unwrap();
        let after: Config = toml::from_str(&text).unwrap();
        assert!(text.contains("# user config"));
        assert!(text.contains("# keep my settings"));
        assert!(text.contains("theme = \"classic\""));
        assert_eq!(after.automation.rule.len(), 5);
        assert_eq!(after.automation.rule[..2], before.automation.rule);
        assert!(!after.automation.enabled);
        assert_eq!(
            after.automation.paused_until,
            before.automation.paused_until
        );
        assert!(after.automation.rule[2..].iter().all(|rule| !rule.enabled));
    }

    #[test]
    fn invalid_config_is_never_overwritten() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        let invalid = "[automation]\nrule = 42\n";
        std::fs::write(&path, invalid).unwrap();
        assert!(install(&path).is_err());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), invalid);
    }

    #[test]
    fn install_creates_config_and_preserves_permissions_on_upgrade() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nested/config.toml");
        assert!(install(&path).unwrap());
        assert!(!install(&path).unwrap());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
                0o600
            );
            std::fs::write(&path, "# old config\n").unwrap();
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o640)).unwrap();
            assert!(install(&path).unwrap());
            assert_eq!(
                std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
                0o640
            );
        }
    }
}
