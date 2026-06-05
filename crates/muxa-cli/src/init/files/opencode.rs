//! `~/.config/opencode/plugins/muxa.ts` content layer.

use crate::init::marker::{self, Outcome};

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
    marker::upsert(original, ID, BODY)
}

pub fn remove(original: &str) -> (String, Outcome) {
    marker::remove(original, ID)
}
