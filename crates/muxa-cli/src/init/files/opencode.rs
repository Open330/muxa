//! `~/.config/opencode/plugins/muxa.ts` content layer.

use crate::init::marker::{self, Outcome, Style};

const ID: &str = "opencode-plugin";

const BODY: &str = r#"// Forward opencode plugin events to muxa without blocking opencode's UI path.
export const MuxaPlugin = async ({ $, client }) => {
  const queue = [];
  let flushing = false;

  const flush = async () => {
    if (flushing) return;
    flushing = true;
    while (queue.length > 0) {
      const event = queue.shift();
      try {
        await $`muxa hook opencode --event event`.stdin(JSON.stringify(event)).quiet();
      } catch (error) {
        try {
          await client.app.log({
            body: {
              service: "muxa",
              level: "warn",
              message: "failed to forward opencode event to muxa",
              extra: { error: String(error) },
            },
          });
        } catch (_) {}
      }
    }
    flushing = false;
  };

  return {
    event: async ({ event }) => {
      queue.push(event);
      if (queue.length > 256) queue.shift();
      setTimeout(flush, 0);
    },
  };
};"#;

pub fn default_path() -> Option<std::path::PathBuf> {
    dirs::config_dir().map(|d| d.join("opencode").join("plugins").join("muxa.ts"))
}

pub fn upsert(original: &str) -> (String, Outcome) {
    // `muxa.ts` is a TS/ESM module, so the managed block must be fenced
    // with `//` line comments — `#`-prefixed fences are a syntax error.
    marker::upsert_styled(original, ID, BODY, Style::Slash)
}

pub fn remove(original: &str) -> (String, Outcome) {
    marker::remove_styled(original, ID, Style::Slash)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generated_plugin_is_valid_ts_comment_fenced() {
        let (out, o) = upsert("");
        assert_eq!(o, Outcome::Inserted);
        // Every fence line is a `//` comment; no bare `#` lines that
        // would break the TS parser.
        assert!(out.contains("// >>> muxa managed (opencode-plugin) >>>"));
        assert!(out.contains("// <<< muxa managed (opencode-plugin) <<<"));
        assert!(!out.lines().any(|l| l.trim_start().starts_with('#')));
        assert!(out.contains("export const MuxaPlugin"));
    }

    #[test]
    fn round_trips_to_empty_on_remove() {
        let (installed, _) = upsert("");
        let (removed, o) = remove(&installed);
        assert_eq!(o, Outcome::Removed);
        assert!(removed.is_empty());
    }
}
