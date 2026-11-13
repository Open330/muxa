//! opencode plugin content layer: `~/.config/opencode/plugins/tend.ts` for
//! opencode 1.x and `~/.config/opencode/plugins/tend/tui.ts` for 2.x.
//!
//! Both forward a filtered subset of opencode's events to
//! `tend hook opencode`; `tend::adapters::opencode` documents the contract.
//! Re-running `tend init` replaces each managed block, so an older plugin
//! body is upgraded in place.
//!
//! **Two files, one install.** opencode 2.x replaced the plugin API and runs
//! sessions in a background server shared by every TUI, so a server plugin
//! there has no pane. Its TUI plugin runs in the pane's own process, which
//! is what tend needs. 1.x loads only `plugins/*.ts`, so it never sees the
//! `tend/` directory; 2.x loads `tend.ts` as a server plugin, which is why
//! that file's default export also satisfies 2.x (`{ id, setup }`, a no-op
//! there). Installing both keeps tend working across an opencode upgrade
//! without re-running `tend init`.
//!
//! **Where.** opencode reads `$XDG_CONFIG_HOME/opencode`, else
//! `~/.config/opencode`, on every platform. Earlier tends wrote the plugin
//! under macOS's `~/Library/Application Support`, which opencode never reads;
//! `tend init` removes that copy.

use crate::init::marker::{self, Outcome, Style};
use std::path::PathBuf;

const ID: &str = "opencode-plugin";
const TUI_ID: &str = "opencode-tui-plugin";

const BODY: &str = r#"// Forward opencode's own events to tend, without blocking opencode's UI path.
//
// Every fact comes from opencode's plugin API: session ids, statuses, tool
// states, permission and question prompts. The plugin only filters and
// de-duplicates; `tend hook opencode` maps the documented shapes to tend
// state. Child sessions (subagent tasks) never reach tend.
import { spawn } from "node:child_process";

// One `tend hook opencode` per payload, on stdin; resolves with its
// stdout. Bun's `$` has no stdin method, so this uses node's spawn, which
// opencode's Bun runtime provides.
const forward = (payload) =>
  new Promise((resolve, reject) => {
    let child;
    try {
      child = spawn("tend", ["hook", "opencode", "--event", "event"], {
        stdio: ["pipe", "pipe", "ignore"],
      });
    } catch (error) {
      reject(error);
      return;
    }
    let out = "";
    child.stdout.on("data", (chunk) => {
      if (out.length < 65536) out += chunk;
    });
    child.on("error", reject);
    child.on("close", () => resolve(out));
    child.stdin.on("error", () => {});
    child.stdin.end(JSON.stringify(payload));
  });

// At a turn boundary `tend hook opencode` may answer `{"prompt": TEXT}`:
// requests the user authorized for this session, offered the way Claude
// Code's Stop hook carries them. TEXT goes to opencode's prompt API.
const offeredPrompt = (out) => {
  try {
    const value = JSON.parse(out);
    return value && typeof value.prompt === "string" && value.prompt.trim() ? value.prompt : undefined;
  } catch (_) {
    return undefined;
  }
};

export const TendPlugin = async ({ client, directory }) => {
  const queue = [];
  let flushing = false;
  const children = new Set();
  const roles = new Map(); // messageID -> "user" | "assistant"
  const replies = new Map(); // sessionID -> { messageID, parts: Map<partID, text> }
  const tools = new Map(); // sessionID:messageID:callID -> last forwarded status
  const completed = new Map(); // assistant messageIDs already reported
  let owner = null; // the session the pane's user last prompted
  const incomplete = new Set(); // sessionIDs whose current turn failed or was aborted
  let prompts = 0; // prompts seen, so an offer never lands after the user typed
  // sessionIDs whose current turn already reported its idle. opencode sends
  // `session.idle` again after an abort's cleanup; only the first idle of a
  // turn may be a boundary, and a failed turn stays failed until the next
  // prompt starts a new one.
  const settled = new Set();

  const remember = (map, key, value, cap = 512) => {
    map.set(key, value);
    if (map.size > cap) map.delete(map.keys().next().value);
  };

  const flush = async () => {
    if (flushing) return;
    flushing = true;
    while (queue.length > 0) {
      const { payload, seq, boundary } = queue.shift();
      try {
        const out = await forward(payload);
        const text = boundary ? offeredPrompt(out) : undefined;
        // The user prompted after this idle: the offer waits for the next
        // boundary instead of racing their turn (nothing is consumed yet).
        if (text && seq === prompts) {
          client.session
            .promptAsync({ path: { id: payload.properties.sessionID }, body: { parts: [{ type: "text", text }] } })
            .catch(() => {});
        }
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

  // `boundary`: a turn that ended cleanly, where tend may offer requests.
  // Esc and failures are not: the user stopped it, or it needs them.
  const send = (type, properties, extra, boundary = false) => {
    const sessionID = properties && properties.sessionID;
    if (sessionID && children.has(sessionID)) return;
    // opencode hands the plugin every session's events in its directory,
    // and every hook call carries this pane. A prompt is the one event that
    // says the pane's user is driving a session, so only a prompt binds the
    // pane, and only the bound session's events are forwarded; another
    // session's late idle or tool event must not revive its row here.
    if (type === "tend.prompt" && sessionID && sessionID !== owner) {
      owner = sessionID;
      queue.push({ payload: { type: "tend.session.active", properties: { sessionID }, tend: { directory } }, seq: prompts });
    }
    if (!sessionID || sessionID !== owner) return;
    const tend = boundary ? { directory, ...extra, boundary: true } : { directory, ...extra };
    queue.push({ payload: { type, properties, tend }, seq: prompts, boundary });
    if (queue.length > 256) queue.shift();
    setTimeout(flush, 0);
  };

  const sessionOf = (event) => {
    const p = event.properties || {};
    return p.sessionID || (p.info && (p.info.sessionID || p.info.id)) || (p.part && p.part.sessionID);
  };

  // Only a turn that ended cleanly reports text: a failed or aborted turn's
  // partial output is not its answer.
  const replyText = (sessionID) => {
    if (incomplete.has(sessionID)) return undefined;
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
      // A new turn: nothing captured earlier belongs to it.
      replies.delete(input.sessionID);
      incomplete.delete(input.sessionID);
      settled.delete(input.sessionID);
      prompts += 1;
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
          // A top-level session is bound by its first prompt, not by its
          // creation: another client in this directory can create one too.
          return;
        case "session.deleted":
          send(event.type, { sessionID });
          if (sessionID === owner) owner = null;
          return;
        case "session.idle": {
          const clean = !incomplete.has(sessionID) && !settled.has(sessionID);
          send(event.type, { sessionID }, { response: replyText(sessionID) }, clean);
          replies.delete(sessionID);
          if (sessionID) settled.add(sessionID);
          return;
        }
        case "session.error":
          if (sessionID) incomplete.add(sessionID);
          send(event.type, p);
          return;
        case "session.status":
          if (p.status && p.status.type === "retry") send(event.type, p);
          return;
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
          if (children.has(info.sessionID)) return;
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
          if (children.has(part.sessionID)) return;
          if (part.type === "text" && roles.get(part.messageID) === "assistant" && !part.synthetic) {
            const reply = replies.get(part.sessionID);
            if (reply && reply.messageID === part.messageID) reply.parts.set(part.id, part.text || "");
            return;
          }
          if (part.type !== "tool" || !part.state) return;
          const status = part.state.status;
          const key = `${part.sessionID}:${part.messageID}:${part.callID}`;
          if (status === "pending" || tools.get(key) === status) return;
          remember(tools, key, status);
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
};

// opencode 1.x runs `server`; 2.x loads this file as a server plugin and
// runs `setup`, which does nothing: its TUI plugin (`tend/tui.ts`) does
// the forwarding there.
export default { id: "tend", server: TendPlugin, setup: () => {} };"#;

const TUI_BODY: &str = r#"// Forward opencode 2.x's own events to tend, from the TUI that runs in this
// terminal pane.
//
// opencode 2.x runs sessions in a background server shared by every TUI, so
// a server plugin has no pane. This TUI plugin does: it runs inside the
// opencode process in the pane, and `tend hook opencode` inherits its
// environment. It translates the 2.x event stream into the payloads
// `tend hook opencode` reads (see `tend::adapters::opencode`).
//
// The pane speaks for one session: the one last prompted while this TUI
// was showing it. Viewing another session moves nothing until it is
// prompted, and events for every other session on the shared server
// (other TUIs, subagents) are dropped.
import { spawn } from "node:child_process";

// One `tend hook opencode` per payload; resolves with its stdout, or ""
// when tend cannot be run.
const run = (payload) =>
  new Promise((resolve) => {
    let child;
    try {
      child = spawn("tend", ["hook", "opencode", "--event", "event"], {
        stdio: ["pipe", "pipe", "ignore"],
      });
    } catch (_) {
      resolve("");
      return;
    }
    let out = "";
    child.stdout.on("data", (chunk) => {
      if (out.length < 65536) out += chunk;
    });
    child.on("error", () => resolve(""));
    child.on("close", () => resolve(out));
    child.stdin.on("error", () => {});
    child.stdin.end(JSON.stringify(payload));
  });

// At a turn boundary `tend hook opencode` may answer `{"prompt": TEXT}`:
// requests the user authorized for this session, offered the way Claude
// Code's Stop hook carries them. TEXT goes to opencode's prompt API.
const offeredPrompt = (out) => {
  try {
    const value = JSON.parse(out);
    return value && typeof value.prompt === "string" && value.prompt.trim() ? value.prompt : undefined;
  } catch (_) {
    return undefined;
  }
};

export default {
  id: "tend",
  setup: async (ctx) => {
    const queue = [];
    let flushing = false;
    let owner = null;
    let reply = null; // { messageID, texts: [] } for the owner's current execution
    let failed = false; // the owner's current execution reported an error
    const tools = new Map(); // tool call id -> name
    let model = null; // the owner's current model, from session.step.started
    // The bound session's directory, from the event that bound it. The
    // TUI's own location follows whatever it shows, which may be another
    // project's session by now.
    let ownerDirectory = null;
    let prompts = 0; // user prompts into the owner, so an offer never lands after the user typed
    const directory = () => {
      const location = ctx.location || {};
      return ownerDirectory || location.directory || process.cwd();
    };

    const flush = async () => {
      if (flushing) return;
      flushing = true;
      while (queue.length > 0) {
        const { payload, seq, boundary } = queue.shift();
        const out = await run(payload);
        const text = boundary ? offeredPrompt(out) : undefined;
        // The user prompted after this idle: the offer waits for the next
        // boundary (nothing is consumed until the model claims it). `queue`
        // delivery never steers a turn that started in between.
        if (text && seq === prompts) {
          try {
            Promise.resolve(
              ctx.client.session.prompt({ sessionID: payload.properties.sessionID, text, delivery: "queue" }),
            ).catch(() => {});
          } catch (_) {}
        }
      }
      flushing = false;
    };
    // `boundary`: a turn that ended cleanly, where tend may offer requests.
    // Esc and failures are not: the user stopped it, or it needs them.
    const send = (type, properties, extra, boundary = false) => {
      const tend = { directory: directory(), ...extra };
      if (boundary) tend.boundary = true;
      queue.push({ payload: { type, properties, tend }, seq: prompts, boundary });
      if (queue.length > 256) queue.shift();
      setTimeout(flush, 0);
    };
    const shown = () => {
      const route = ctx.ui.router.current();
      return route && route.type === "session" ? route.sessionID : null;
    };
    const replyText = () => {
      const text = reply ? reply.texts.join("\n").trim() : "";
      return text.length > 0 ? text : undefined;
    };

    const unlisten = ctx.data.listen(({ details: event }) => {
      const d = (event && event.data) || {};
      const sessionID = d.sessionID || (d.form && d.form.sessionID);
      if (event.type === "session.inbox.enqueued") {
        const item = d.item || {};
        if (item.type === "user" && sessionID && sessionID === owner) prompts += 1;
        // A prompt into the session this TUI shows binds the pane to it.
        if (item.type !== "user" || !sessionID || sessionID !== shown()) return;
        if (sessionID !== owner) prompts += 1;
        if (sessionID !== owner) {
          owner = sessionID;
          ownerDirectory = (event.location && event.location.directory) || null;
          // Nothing captured for the previous session belongs to this one.
          reply = null;
          failed = false;
          model = null;
          send("tend.session.active", { sessionID });
        }
        const text = item.payload && typeof item.payload.text === "string" ? item.payload.text : "";
        send("tend.prompt", { sessionID, text: text.trim() });
        return;
      }
      if (!owner || sessionID !== owner) return;
      switch (event.type) {
        case "session.execution.started":
          reply = null;
          failed = false;
          // Work resumes even when no prompt precedes it here (one queued
          // during the previous execution's cleanup), so say so.
          send("tend.execution.started", { sessionID });
          return;
        case "session.text.ended":
          if (!reply || reply.messageID !== d.assistantMessageID) {
            reply = { messageID: d.assistantMessageID, texts: [] };
          }
          if (typeof d.text === "string") reply.texts.push(d.text);
          return;
        case "session.tool.input.started":
          tools.set(d.id, d.name);
          if (tools.size > 512) tools.delete(tools.keys().next().value);
          return;
        case "session.tool.called": {
          const input = d.input || {};
          send("message.part.updated", {
            sessionID,
            part: {
              type: "tool",
              tool: tools.get(d.id) || "tool",
              callID: d.id,
              state: { status: "running", command: typeof input.command === "string" ? input.command : undefined },
            },
          });
          return;
        }
        case "session.tool.success":
        case "session.tool.failed":
          send("message.part.updated", {
            sessionID,
            part: {
              type: "tool",
              tool: tools.get(d.id) || "tool",
              callID: d.id,
              state: { status: event.type === "session.tool.success" ? "completed" : "error" },
            },
          });
          return;
        case "session.step.started":
          if (d.model) model = d.model;
          return;
        case "session.step.ended":
          send("message.updated", {
            sessionID,
            info: {
              id: d.assistantMessageID,
              role: "assistant",
              modelID: model && model.id,
              providerID: model && model.providerID,
              cost: d.cost,
            },
          });
          return;
        case "session.retry.scheduled":
          send("session.status", {
            sessionID,
            status: { type: "retry", attempt: d.attempt, message: d.error && d.error.message },
          });
          return;
        case "permission.asked":
          send("permission.asked", { sessionID, id: d.id, action: d.action, resources: d.resources });
          return;
        case "permission.replied":
          send("permission.replied", { sessionID, requestID: d.requestID, reply: d.reply });
          return;
        case "form.created": {
          const fields = (d.form && d.form.fields) || [];
          send("question.asked", {
            sessionID,
            id: d.form && d.form.id,
            questions: fields.map((field) => ({ question: field.description || field.title, header: field.title })),
          });
          return;
        }
        case "form.replied":
        case "form.cancelled":
          send(event.type === "form.replied" ? "question.replied" : "question.rejected", { sessionID, requestID: d.id });
          return;
        case "session.execution.failed":
          failed = true;
          send("session.error", {
            sessionID,
            error: { name: (d.error && d.error.type) || "error", data: { message: d.error && d.error.message } },
          });
          send("session.idle", { sessionID });
          reply = null;
          return;
        case "session.execution.interrupted":
          // Esc, a rejected permission, or a superseded turn: the session
          // is idle again with no answer to report.
          send("session.idle", { sessionID });
          reply = null;
          return;
        case "session.execution.succeeded":
          send("session.idle", { sessionID }, failed ? {} : { response: replyText() }, !failed);
          reply = null;
          failed = false;
          return;
        case "session.deleted":
          send("session.deleted", { sessionID });
          owner = null;
          return;
        default:
          return;
      }
    });
    return () => unlisten();
  },
};"#;

/// opencode's config directory: `$XDG_CONFIG_HOME/opencode`, else
/// `~/.config/opencode`, on every platform (opencode's `global-roots`).
pub fn config_dir() -> Option<PathBuf> {
    config_dir_from(
        std::env::var_os("XDG_CONFIG_HOME").map(PathBuf::from),
        dirs::home_dir(),
    )
}

fn config_dir_from(xdg: Option<PathBuf>, home: Option<PathBuf>) -> Option<PathBuf> {
    xdg.filter(|dir| dir.is_absolute())
        .or_else(|| home.map(|home| home.join(".config")))
        .map(|dir| dir.join("opencode"))
}

/// The opencode 1.x plugin file.
pub fn default_path() -> Option<PathBuf> {
    config_dir().map(|d| d.join("plugins").join("tend.ts"))
}

/// The opencode 2.x TUI plugin, a local plugin directory's `tui` entry.
pub fn tui_path() -> Option<PathBuf> {
    config_dir().map(|d| d.join("plugins").join("tend").join("tui.ts"))
}

/// Where earlier tends wrote the plugin on macOS (`dirs::config_dir()` is
/// `~/Library/Application Support` there). opencode never read it. `None`
/// when it is the same path as [`default_path`], as on Linux.
pub fn misplaced_path() -> Option<PathBuf> {
    let old = dirs::config_dir()?
        .join("opencode")
        .join("plugins")
        .join("tend.ts");
    (Some(&old) != default_path().as_ref()).then_some(old)
}

pub fn upsert(original: &str) -> (String, Outcome) {
    // `tend.ts` is a TS/ESM module, so the managed block must be fenced
    // with `//` line comments — `#`-prefixed fences are a syntax error.
    marker::upsert_styled(original, ID, BODY, Style::Slash)
}

pub fn remove(original: &str) -> (String, Outcome) {
    marker::remove_styled(original, ID, Style::Slash)
}

pub fn upsert_tui(original: &str) -> (String, Outcome) {
    marker::upsert_styled(original, TUI_ID, TUI_BODY, Style::Slash)
}

pub fn remove_tui(original: &str) -> (String, Outcome) {
    marker::remove_styled(original, TUI_ID, Style::Slash)
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
        assert!(BODY.contains(r#"["hook", "opencode", "--event", "event"]"#));
        // Bun's `$` has no `.stdin()`; every forward failed against a real
        // opencode 1.19.5 until the plugin spawned tend itself.
        assert!(!BODY.contains(".stdin("));
    }

    /// The 2.x TUI plugin sends the same vocabulary, translated from the
    /// 2.x event stream it listens to.
    #[test]
    fn tui_plugin_translates_the_2x_events_the_adapter_needs() {
        for ty in [
            "tend.session.active",
            "tend.prompt",
            "session.idle",
            "session.error",
            "session.status",
            "session.deleted",
            "permission.asked",
            "permission.replied",
            "question.asked",
            "question.replied",
            "question.rejected",
            "message.updated",
            "message.part.updated",
        ] {
            assert!(
                TUI_BODY.contains(&format!("\"{ty}\"")),
                "tui plugin never sends {ty}"
            );
        }
        for source in [
            "session.inbox.enqueued",
            "session.execution.started",
            "session.execution.succeeded",
            "session.execution.failed",
            "session.execution.interrupted",
            "session.tool.called",
            "session.tool.success",
            "session.tool.failed",
            "form.created",
        ] {
            assert!(
                TUI_BODY.contains(&format!("\"{source}\"")),
                "tui plugin ignores {source}"
            );
        }
        assert!(TUI_BODY.contains("export default"));
        assert!(TUI_BODY.contains(r#"["hook", "opencode", "--event", "event"]"#));
        let (out, o) = upsert_tui("");
        assert_eq!(o, Outcome::Inserted);
        assert!(!out.lines().any(|l| l.trim_start().starts_with('#')));
        let (removed, o) = remove_tui(&out);
        assert_eq!(o, Outcome::Removed);
        assert!(removed.is_empty());
    }

    /// One `tend.ts` must load under both plugin APIs: 1.x takes
    /// `default.server`, 2.x requires `default.{id, setup}`.
    #[test]
    fn v1_plugin_default_export_satisfies_both_loaders() {
        assert!(
            BODY.contains("export default { id: \"tend\", server: TendPlugin, setup: () => {} };")
        );
    }

    #[test]
    fn config_dir_follows_xdg_then_home_config_on_every_platform() {
        assert_eq!(
            config_dir_from(Some("/x".into()), Some("/h".into())),
            Some(PathBuf::from("/x/opencode"))
        );
        // A relative XDG_CONFIG_HOME is invalid per the spec; opencode's
        // `||` would take it, but tend never writes to a cwd-relative path.
        assert_eq!(
            config_dir_from(Some("rel".into()), Some("/h".into())),
            Some(PathBuf::from("/h/.config/opencode"))
        );
        assert_eq!(
            config_dir_from(None, Some("/h".into())),
            Some(PathBuf::from("/h/.config/opencode"))
        );
    }

    #[test]
    fn round_trips_to_empty_on_remove() {
        let (installed, _) = upsert("");
        let (removed, o) = remove(&installed);
        assert_eq!(o, Outcome::Removed);
        assert!(removed.is_empty());
    }
}
