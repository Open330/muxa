//! CLI and MCP entry points share one local, read-only transcript engine.
use anyhow::{anyhow, Result};
use clap::Args;
use muxa::agent_records::{ReadRequest, RecordStore, SearchRequest};
use serde_json::{json, Value};
use std::path::PathBuf;

#[derive(Debug, Clone, Args)]
pub struct SearchArgs {
    /// Natural-language question (Korean/English), identifier, or keywords.
    pub query: Option<String>,
    /// Supplement the question with explicit alternate keywords (repeatable).
    #[arg(long = "term")]
    pub terms: Vec<String>,
    /// Inclusive event timestamp, RFC3339 with explicit offset.
    #[arg(long)]
    pub from: Option<String>,
    /// Exclusive event timestamp, RFC3339 with explicit offset.
    #[arg(long)]
    pub to: Option<String>,
    /// Timezone for dates in the question: UTC, KST, Asia/Seoul or ±HH:MM.
    #[arg(long, default_value = "UTC")]
    pub timezone: String,
    /// claude or codex (repeatable; default both).
    #[arg(long = "provider", value_parser = ["claude", "codex"])]
    pub providers: Vec<String>,
    #[arg(long)]
    pub session_id: Option<String>,
    /// Exact recorded working directory.
    #[arg(long)]
    pub cwd: Option<String>,
    #[arg(long = "kind", value_parser = ["user", "assistant", "tool_call", "tool_result", "instruction"])]
    pub kinds: Vec<String>,
    /// Exclude the investigating session to avoid matching its own investigation.
    #[arg(long = "exclude-session")]
    pub exclude_session_ids: Vec<String>,
    #[arg(long)]
    pub include_instructions: bool,
    #[arg(long, default_value_t = 20)]
    pub limit: usize,
    #[arg(long, default_value_t = 0)]
    pub offset: usize,
    /// Additional transcript root (repeatable). Provider defaults remain included.
    #[arg(long = "root")]
    pub roots: Vec<PathBuf>,
    /// Search only --root paths instead of including default provider homes.
    #[arg(long, requires = "roots")]
    pub only_roots: bool,
    /// Maximum new source bytes to index; prefix verification is additional I/O.
    #[arg(long, default_value_t = 134_217_728)]
    pub max_index_bytes: u64,
    #[arg(long)]
    pub json: bool,
}

#[derive(Debug, Clone, Args)]
pub struct ReadArgs {
    /// Opaque `event_id` returned by agent search.
    pub event_id: String,
    #[arg(long, default_value_t = 5)]
    pub before: usize,
    #[arg(long, default_value_t = 10)]
    pub after: usize,
    #[arg(long)]
    pub no_tool_pair: bool,
    #[arg(long, default_value_t = 0)]
    pub text_offset: usize,
    #[arg(long, default_value_t = 24_000)]
    pub max_chars: usize,
    #[arg(long)]
    pub json: bool,
}

pub fn search(args: SearchArgs) -> Result<()> {
    let request = SearchRequest {
        query: args.query.unwrap_or_default(),
        terms: args.terms,
        from: args.from,
        to: args.to,
        timezone: args.timezone,
        providers: args.providers,
        session_id: args.session_id,
        cwd: args.cwd,
        kinds: args.kinds,
        exclude_session_ids: args.exclude_session_ids,
        include_instructions: args.include_instructions,
        limit: args.limit,
        offset: args.offset,
        roots: args.roots,
        only_roots: args.only_roots,
        max_index_bytes: args.max_index_bytes,
    };
    let result = RecordStore::for_search(&request)
        .and_then(|s| s.search(&request))
        .map_err(|e| anyhow!(e))?;
    if args.json {
        println!("{}", serde_json::to_string(&result)?);
    } else {
        println!(
            "Search terms: {}\nExpanded: {}\nTime: {} .. {} ({})",
            result.plan.terms.join(", "),
            result.plan.expanded_terms.join(", "),
            result.plan.from.as_deref().unwrap_or("any"),
            result.plan.to.as_deref().unwrap_or("any"),
            result.plan.timezone
        );
        println!(
            "Coverage: {}/{} files current; complete={}",
            result.coverage.files_current,
            result.coverage.files_discovered,
            result.coverage.complete
        );
        for hit in &result.matches {
            let e = &hit.event;
            println!(
                "\n{} {} {} [{}]\n{}:{}\nevent_id={}\n{}",
                e.timestamp.as_deref().unwrap_or("unknown time"),
                e.provider,
                e.session_id,
                e.kind,
                e.path,
                e.line,
                e.event_id,
                e.text
            );
        }
        for warning in &result.coverage.warnings {
            eprintln!("Warning: {warning}");
        }
        if let Some(offset) = result.next_offset {
            println!("\nMore results: --offset {offset}");
        }
        println!("\nRead context: muxa agent read <event_id>. No match does not prove absence.");
    }
    Ok(())
}

pub fn read(args: ReadArgs) -> Result<()> {
    let request = ReadRequest {
        event_id: args.event_id,
        before: args.before,
        after: args.after,
        include_tool_pair: !args.no_tool_pair,
        text_offset: args.text_offset,
        max_chars: args.max_chars,
    };
    let result = RecordStore::local(&[])
        .and_then(|s| s.read(&request))
        .map_err(|e| anyhow!(e))?;
    if args.json {
        println!("{}", serde_json::to_string(&result)?);
    } else {
        for e in result.events.iter().chain(&result.related) {
            println!(
                "\n{} [{}] {}:{}\nevent_id={}\n{}{}",
                e.timestamp.as_deref().unwrap_or("unknown time"),
                e.kind,
                e.path,
                e.line,
                e.event_id,
                e.text,
                if e.truncated { "\n[truncated]" } else { "" }
            );
        }
        if let Some(offset) = result.next_text_offset {
            println!("\nContinue anchor text: --text-offset {offset}");
        }
        println!("\n{}", result.guidance);
    }
    Ok(())
}

pub fn definitions() -> Vec<Value> {
    vec![
        json!({
            "name":"muxa_agent_search",
            "description":"Search local Claude/Codex conversation and command evidence using a Korean/English natural-language question, identifiers, or keywords. Uses transparent local keyword/intent expansion, not embeddings or a model. Returns the interpreted terms/time range, ranked excerpts, event_ids, source lines, and coverage. Dates refer to event timestamps, never file creation dates. To investigate broadly, follow hits with muxa_agent_read, vary terms/time/session filters, and inspect calls AND results. No match is not proof of absence; repeat when indexing is incomplete. Read-only on provider files; builds a private disposable local index. No network calls.",
            "inputSchema":{"type":"object","properties":{
                "query":{"type":"string","description":"Question, e.g. 2026.10.22 11:54 KST에 callabo-internal 버킷을 우리가 만들었는지. Date + time means ±15 minutes; date alone means that day. Explicit from/to override inferred bounds."},
                "terms":{"type":"array","items":{"type":"string"},"maxItems":24,"description":"Supplement with alternate literal terms; OR retrieval allows broader investigation."},
                "from":{"type":"string","description":"Inclusive RFC3339 event timestamp with timezone."},
                "to":{"type":"string","description":"Exclusive RFC3339 event timestamp with timezone."},
                "timezone":{"type":"string","description":"For dates in query: UTC (default), KST, Asia/Seoul, or ±HH:MM."},
                "providers":{"type":"array","items":{"type":"string","enum":["claude","codex"]}},
                "session_id":{"type":"string","description":"Exact historical session; omit keywords to browse its timeline."},
                "cwd":{"type":"string","description":"Exact historical working directory."},
                "kinds":{"type":"array","items":{"type":"string","enum":["user","assistant","tool_call","tool_result","instruction"]}},
                "exclude_session_ids":{"type":"array","items":{"type":"string"},"maxItems":100,"description":"Exclude your own investigating session and known copied conversations."},
                "include_instructions":{"type":"boolean","description":"Include auto-attached instructions and skill text. Default false."},
                "limit":{"type":"integer","minimum":1,"maximum":100,"default":20},
                "offset":{"type":"integer","minimum":0,"maximum":1900,"default":0},
                "roots":{"type":"array","items":{"type":"string"},"description":"Additional local transcript directories; defaults, provider env homes and discovered ASX profiles remain included."},
            "only_roots":{"type":"boolean","default":false,"description":"Search only explicit roots; requires roots. Useful to focus on selected files/directories without indexing every provider home."},
            "max_index_bytes":{"type":"integer","minimum":1,"maximum":1_073_741_824,"default":134_217_728,"description":"New-source-byte budget this call; prefix verification is additional I/O. Discovery/indexing is also bounded to ~12 seconds. Repeat calls continue from saved positions."}
            },"additionalProperties":false}
        }),
        json!({
            "name":"muxa_agent_read",
            "description":"Read source-attributed events around a muxa_agent_search event_id, with paired tool invocations/results even when separated by other messages. Use this to verify what was requested, claimed, invoked and returned. Prose and quoted logs are not proof of execution. Output is bounded and secrets/signed URL queries are redacted. Follow another event_id to widen the investigation or use text_offset to continue a clipped anchor.",
            "inputSchema":{"type":"object","properties":{
                "event_id":{"type":"string"},
                "before":{"type":"integer","minimum":0,"maximum":50,"default":5},
                "after":{"type":"integer","minimum":0,"maximum":50,"default":10},
                "include_tool_pair":{"type":"boolean","default":true},
                "text_offset":{"type":"integer","minimum":0,"default":0},
                "max_chars":{"type":"integer","minimum":1,"maximum":64000,"default":24000}
            },"required":["event_id"],"additionalProperties":false}
        }),
    ]
}

pub async fn call(name: &str, args: Value) -> Result<Value> {
    let name = name.to_owned();
    tokio::task::spawn_blocking(move || -> Result<Value> {
        if name == "muxa_agent_search" {
            let request: SearchRequest = serde_json::from_value(args)?;
            let result = RecordStore::for_search(&request)
                .and_then(|s| s.search(&request))
                .map_err(|e| anyhow!(e))?;
            Ok(serde_json::to_value(result)?)
        } else {
            let request: ReadRequest = serde_json::from_value(args)?;
            let result = RecordStore::local(&[])
                .and_then(|s| s.read(&request))
                .map_err(|e| anyhow!(e))?;
            Ok(serde_json::to_value(result)?)
        }
    })
    .await?
}
