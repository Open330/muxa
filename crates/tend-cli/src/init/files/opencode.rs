//! `~/.config/opencode/plugins/tend.ts` content layer.
//!
//! The plugin forwards a filtered subset of opencode's events to
//! `tend hook opencode`; `tend::adapters::opencode` documents the contract
//! between the two. Re-running `tend init` replaces the managed block, so an
//! older plugin body is upgraded in place.

use crate::init::marker::{self, Outcome, Style};

const ID: &str = "opencode-plugin";

const BODY: &str = r#"// Forward opencode's own events to tend, without blocking opencode's UI path.
//
// Every fact comes from opencode's plugin API: session ids, statuses, tool
// states, permission and question prompts. The plugin only filters and
// de-duplicates; `tend hook opencode` maps the documented shapes to tend
// state. Child sessions (subagent tasks) never reach tend, so a pane shows
// the conversation its user is driving.
export const TendPlugin = async ({ $, client, directory }) => {
  const queue = [];
  let flushing = false;
  const children = new Set();
  const roles = new Map(); // messageID -> "user" | "assistant"
  const replies = new Map(); // sessionID -> { messageID, parts: Map<partID, text> }
  const tools = new Map(); // callID -> last forwarded status
  const completed = new Map(); // assistant messageIDs already reported
  let active = null;

  const remember = (map, key, value, cap = 512) => {
    map.set(key, value);
    if (map.size > cap) map.delete(map.keys().next().value);
  };

  const flush = async () => {
    if (flushing) return;
    flushing = true;
    while (queue.length > 0) {
      const payload = queue.shift();
      try {
        await $`tend hook opencode --event event`.stdin(JSON.stringify(payload)).quiet();
      } catch (error) {
        try {
          await client.app.log({
            body: {
              service: "tend",
              level: "warn",
              message: "failed to forward opencode event to tend",
              extra: { error: String(error) },
            },
          });
        } catch (_) {}
      }
    }
    flushing = false;
  };

  const send = (type, properties, extra) => {
    const sessionID = properties && properties.sessionID;
    if (sessionID && children.has(sessionID)) return;
    // The pane follows the session that is producing events, so switching
    // sessions in the TUI moves the pane's row instead of leaving two.
    if (sessionID && sessionID !== active && type !== "session.deleted") {
      active = sessionID;
      queue.push({ type: "tend.session.active", properties: { sessionID }, tend: { directory } });
    }
    queue.push({ type, properties, tend: { directory, ...extra } });
    if (queue.length > 256) queue.shift();
    setTimeout(flush, 0);
  };

  const sessionOf = (event) => {
    const p = event.properties || {};
    return p.sessionID || (p.info && (p.info.sessionID || p.info.id)) || (p.part && p.part.sessionID);
  };

  const replyText = (sessionID) => {
    const reply = replies.get(sessionID);
    if (!reply) return undefined;
    const text = [...reply.parts.values()].join("\n").trim();
    return text.length > 0 ? text : undefined;
  };

  return {
    "chat.message": async (input, output) => {
      const text = (output.parts || [])
        .filter((part) => part.type === "text" && !part.synthetic && !part.ignored)
        .map((part) => part.text)
        .join("\n")
        .trim();
      send("tend.prompt", { sessionID: input.sessionID, text });
    },
    event: async ({ event }) => {
      const p = event.properties || {};
      const sessionID = sessionOf(event);
      switch (event.type) {
        case "session.created":
        case "session.updated":
          if (p.info && p.info.parentID) {
            children.add(p.info.id);
            return;
          }
          if (event.type === "session.created") {
            send(event.type, { sessionID, info: { id: p.info.id, directory: p.info.directory } });
          }
          return;
        case "session.deleted":
          if (sessionID === active) active = null;
          send(event.type, { sessionID });
          return;
        case "session.idle":
          send(event.type, { sessionID }, { response: replyText(sessionID) });
          return;
        case "session.status":
          if (p.status && p.status.type === "retry") send(event.type, p);
          return;
        case "session.error":
        case "permission.updated":
        case "permission.asked":
        case "permission.replied":
        case "permission.v2.asked":
        case "permission.v2.replied":
        case "question.asked":
        case "question.replied":
        case "question.rejected":
        case "question.v2.asked":
        case "question.v2.replied":
        case "question.v2.rejected":
          send(event.type, p);
          return;
        case "message.updated": {
          const info = p.info || {};
          remember(roles, info.id, info.role);
          if (info.role === "assistant") {
            const reply = replies.get(info.sessionID);
            if (!reply || reply.messageID !== info.id) {
              remember(replies, info.sessionID, { messageID: info.id, parts: new Map() }, 64);
            }
            if (info.time && info.time.completed && !completed.has(info.id)) {
              remember(completed, info.id, true);
              send(event.type, {
                sessionID: info.sessionID,
                info: { id: info.id, role: info.role, modelID: info.modelID, providerID: info.providerID, cost: info.cost },
              });
            }
          }
          return;
        }
        case "message.part.updated": {
          const part = p.part || {};
          if (part.type === "text" && roles.get(part.messageID) === "assistant" && !part.synthetic) {
            const reply = replies.get(part.sessionID);
            if (reply && reply.messageID === part.messageID) reply.parts.set(part.id, part.text || "");
            return;
          }
          if (part.type !== "tool" || !part.state) return;
          const status = part.state.status;
          if (status === "pending" || tools.get(part.callID) === status) return;
          remember(tools, part.callID, status);
          const input = part.state.input || {};
          send(event.type, {
            sessionID: part.sessionID,
            part: {
              type: "tool",
              tool: part.tool,
              callID: part.callID,
              state: { status, command: typeof input.command === "string" ? input.command : undefined },
            },
          });
          return;
        }
        default:
          return;
      }
    },
  };
};"#;

pub fn default_path() -> Option<std::path::PathBuf> {
    dirs::config_dir().map(|d| d.join("opencode").join("plugins").join("tend.ts"))
}

pub fn upsert(original: &str) -> (String, Outcome) {
    // `tend.ts` is a TS/ESM module, so the managed block must be fenced
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
        assert!(out.contains("// >>> tend managed (opencode-plugin) >>>"));
        assert!(out.contains("// <<< tend managed (opencode-plugin) <<<"));
        assert!(!out.lines().any(|l| l.trim_start().starts_with('#')));
        assert!(out.contains("export const TendPlugin"));
    }

    /// The events the plugin forwards are exactly the ones the adapter maps;
    /// a type added on one side and not the other would be dropped silently.
    #[test]
    fn plugin_forwards_the_event_types_the_adapter_maps() {
        for ty in [
            "tend.session.active",
            "tend.prompt",
            "session.created",
            "session.deleted",
            "session.idle",
            "session.status",
            "session.error",
            "permission.updated",
            "permission.asked",
            "permission.replied",
            "permission.v2.asked",
            "permission.v2.replied",
            "question.asked",
            "question.replied",
            "question.rejected",
            "message.updated",
            "message.part.updated",
        ] {
            assert!(
                BODY.contains(&format!("\"{ty}\"")),
                "plugin never sends {ty}"
            );
        }
        assert!(BODY.contains("parentID"), "child sessions must be filtered");
        assert!(BODY.contains("tend hook opencode --event event"));
    }

    #[test]
    fn round_trips_to_empty_on_remove() {
        let (installed, _) = upsert("");
        let (removed, o) = remove(&installed);
        assert_eq!(o, Outcome::Removed);
        assert!(removed.is_empty());
    }
}
