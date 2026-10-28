//! Searchable local Claude/Codex evidence, independent of the live daemon.
//! The cache is disposable: provider JSONL files remain the source of truth.
mod index;
mod parse;
use index::{index_file, source_current};
mod query;

pub use parse::Event;
pub use query::QueryPlan;
use rusqlite::{params, params_from_iter, Connection};
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::fmt::Write as _;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};
use time::OffsetDateTime;

type Result<T> = std::result::Result<T, String>;
const DEFAULT_INDEX_BYTES: u64 = 128 * 1024 * 1024;
const MAX_DISCOVERY_ENTRIES: usize = 250_000;
const MAX_LINE: usize = 8 * 1024 * 1024;
const CACHE_SCHEMA_VERSION: u32 = 4;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct SearchRequest {
    pub query: String,
    /// Explicit keywords supplement natural-language planning, for follow-up searches.
    pub terms: Vec<String>,
    pub from: Option<String>,
    pub to: Option<String>,
    pub timezone: String,
    pub providers: Vec<String>,
    pub session_id: Option<String>,
    pub cwd: Option<String>,
    pub kinds: Vec<String>,
    pub exclude_session_ids: Vec<String>,
    pub include_instructions: bool,
    pub limit: usize,
    pub offset: usize,
    /// Additional transcript directories; defaults are still discovered.
    pub roots: Vec<PathBuf>,
    /// Search only explicit roots, for a targeted investigation or isolated test.
    pub only_roots: bool,
    pub max_index_bytes: u64,
}

impl Default for SearchRequest {
    fn default() -> Self {
        Self {
            query: String::new(),
            terms: Vec::new(),
            from: None,
            to: None,
            timezone: "UTC".into(),
            providers: Vec::new(),
            session_id: None,
            cwd: None,
            kinds: Vec::new(),
            exclude_session_ids: Vec::new(),
            include_instructions: false,
            limit: 20,
            offset: 0,
            roots: Vec::new(),
            only_roots: false,
            max_index_bytes: DEFAULT_INDEX_BYTES,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ReadRequest {
    pub event_id: String,
    pub before: usize,
    pub after: usize,
    pub include_tool_pair: bool,
    /// Continue a truncated event, in Unicode characters, without reprinting it all.
    pub text_offset: usize,
    pub max_chars: usize,
}

impl Default for ReadRequest {
    fn default() -> Self {
        Self {
            event_id: String::new(),
            before: 5,
            after: 10,
            include_tool_pair: true,
            text_offset: 0,
            max_chars: 24_000,
        }
    }
}

#[derive(Debug, Default, Serialize)]
pub struct Coverage {
    pub roots: Vec<PathBuf>,
    pub files_discovered: usize,
    pub files_current: usize,
    pub files_failed: usize,
    pub bytes_read: u64,
    pub index_bytes: u64,
    pub malformed_lines: u64,
    pub oversized_lines: u64,
    pub unsupported_records: u64,
    pub truncated_events: u64,
    pub untimed_events: u64,
    /// False also when source discovery or parsing failed; zero hits is not absence.
    pub complete: bool,
    pub warnings: Vec<String>,
}

#[derive(Debug, Serialize)]
pub struct Hit {
    #[serde(flatten)]
    pub event: Event,
    pub score: f64,
    pub matched_terms: Vec<String>,
}

#[derive(Debug, Serialize)]
pub struct SearchResult {
    pub plan: QueryPlan,
    pub matches: Vec<Hit>,
    pub has_more: bool,
    pub candidates_limited: bool,
    pub next_offset: Option<usize>,
    pub coverage: Coverage,
    pub guidance: Vec<String>,
}

#[derive(Debug, Serialize)]
#[allow(clippy::struct_excessive_bools)] // Independent pagination, cache freshness and source availability facts.
pub struct ReadResult {
    pub events: Vec<Event>,
    pub related: Vec<Event>,
    pub has_before: bool,
    pub has_after: bool,
    pub next_text_offset: Option<usize>,
    pub source_available: bool,
    pub source_current: bool,
    pub related_truncated: bool,
    pub guidance: &'static str,
}

pub struct RecordStore {
    cache: PathBuf,
    roots: Vec<PathBuf>,
    legacy_cache: Option<PathBuf>,
}

impl RecordStore {
    pub fn new(cache: PathBuf, roots: Vec<PathBuf>) -> Self {
        Self {
            cache,
            roots,
            legacy_cache: None,
        }
    }

    pub fn for_search(request: &SearchRequest) -> Result<Self> {
        if request.only_roots && request.roots.is_empty() {
            return Err("only_roots requires at least one root".into());
        }
        let mut store = Self::local(&request.roots)?;
        if request.only_roots {
            store.roots.clone_from(&request.roots);
        }
        Ok(store)
    }

    pub fn local(extra: &[PathBuf]) -> Result<Self> {
        for path in extra {
            if !path.exists() {
                return Err(format!(
                    "additional transcript root does not exist: {}",
                    path.display()
                ));
            }
        }
        let home = dirs::home_dir().ok_or("cannot locate home directory")?;
        let mut roots = vec![
            home.join(".claude/projects"),
            home.join(".codex/sessions"),
            home.join(".codex/archived_sessions"),
        ];
        if let Some(path) = std::env::var_os("CODEX_HOME") {
            roots.extend([
                PathBuf::from(&path).join("sessions"),
                PathBuf::from(path).join("archived_sessions"),
            ]);
        }
        if let Some(path) = std::env::var_os("CLAUDE_CONFIG_DIR") {
            roots.push(PathBuf::from(path).join("projects"));
        }
        // Optional account-switcher homes. Symlinks and overlapping roots are
        // canonicalized during discovery, so one transcript is indexed once.
        if let Ok(profiles) = fs::read_dir(home.join(".config/asx/profiles")) {
            for profile in profiles.flatten() {
                roots.extend([
                    profile.path().join("projects"),
                    profile.path().join("sessions"),
                    profile.path().join("archived_sessions"),
                ]);
            }
        }
        roots.extend_from_slice(extra);
        let cache = crate::paths::data_dir()
            .ok_or("cannot locate Muxa data directory")?
            .join("muxa/agent-records-v2.sqlite");
        let mut store = Self::new(cache, roots);
        store.legacy_cache = store
            .cache
            .parent()
            .map(|p| p.join("agent-records-v1.sqlite"));
        Ok(store)
    }

    fn connect(&self) -> Result<Connection> {
        if let Some(legacy) = &self.legacy_cache {
            // v1 may contain credentials indexed before structured redaction.
            // It is a disposable derived cache, never a provider transcript.
            for suffix in ["", "-wal", "-shm", "-journal"] {
                let path = PathBuf::from(format!("{}{suffix}", legacy.display()));
                match fs::remove_file(path) {
                    Ok(()) => {}
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                    Err(e) => return Err(format!("retiring legacy record cache: {e}")),
                }
            }
        }
        if let Some(parent) = self.cache.parent() {
            fs::create_dir_all(parent).map_err(|e| e.to_string())?;
        }
        let mut options = fs::OpenOptions::new();
        options.create(true).append(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
            options.mode(0o600);
            options.open(&self.cache).map_err(|e| e.to_string())?;
            fs::set_permissions(&self.cache, fs::Permissions::from_mode(0o600))
                .map_err(|e| e.to_string())?;
        }
        #[cfg(not(unix))]
        options.open(&self.cache).map_err(|e| e.to_string())?;
        let mut conn = Connection::open(&self.cache).map_err(|e| e.to_string())?;
        conn.busy_timeout(Duration::from_secs(30))
            .map_err(|e| e.to_string())?;
        conn.execute_batch("PRAGMA foreign_keys=ON;PRAGMA secure_delete=ON;")
            .map_err(|e| e.to_string())?;
        let journal: String =
            sqlite_retry(|| conn.query_row("PRAGMA journal_mode", [], |r| r.get(0)))?;
        if journal != "wal" {
            sqlite_retry(|| conn.pragma_update(None, "journal_mode", "WAL"))?;
        }
        let version: u32 = conn
            .query_row("PRAGMA user_version", [], |r| r.get(0))
            .map_err(|e| e.to_string())?;
        if version > CACHE_SCHEMA_VERSION {
            return Err("record cache was created by a newer Muxa version".into());
        }
        if version != CACHE_SCHEMA_VERSION {
            let tx = conn
                .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
                .map_err(|e| e.to_string())?;
            let version: u32 = tx
                .query_row("PRAGMA user_version", [], |r| r.get(0))
                .map_err(|e| e.to_string())?;
            if version > CACHE_SCHEMA_VERSION {
                return Err("record cache was created by a newer Muxa version".into());
            }
            if version != CACHE_SCHEMA_VERSION {
                tx.execute_batch("DROP TRIGGER IF EXISTS records_insert;DROP TRIGGER IF EXISTS records_delete;
                    DROP TABLE IF EXISTS records_fts;DROP TABLE IF EXISTS source_chunks;DROP TABLE IF EXISTS events;DROP TABLE IF EXISTS sources;
                    CREATE TABLE sources(path TEXT PRIMARY KEY,size INTEGER NOT NULL,modified TEXT NOT NULL,offset INTEGER NOT NULL,line INTEGER NOT NULL,context TEXT NOT NULL,
                        open_tail INTEGER NOT NULL DEFAULT 0,skipping INTEGER NOT NULL DEFAULT 0,last_attempt INTEGER NOT NULL DEFAULT 0,malformed INTEGER NOT NULL DEFAULT 0,oversized INTEGER NOT NULL DEFAULT 0,unsupported INTEGER NOT NULL DEFAULT 0,truncated INTEGER NOT NULL DEFAULT 0);
                    CREATE TABLE events(id TEXT PRIMARY KEY,source TEXT NOT NULL REFERENCES sources(path) ON DELETE CASCADE,line INTEGER NOT NULL,part INTEGER NOT NULL,ts INTEGER,provider TEXT NOT NULL,session_id TEXT NOT NULL,cwd TEXT,kind TEXT NOT NULL,call_id TEXT,text TEXT NOT NULL,metadata TEXT NOT NULL);
                    CREATE INDEX events_time ON events(ts);CREATE INDEX events_session ON events(provider,session_id,source,line,part);CREATE INDEX events_call ON events(provider,session_id,call_id);
                    CREATE TABLE source_chunks(source TEXT NOT NULL REFERENCES sources(path) ON DELETE CASCADE,start INTEGER NOT NULL,len INTEGER NOT NULL,digest TEXT NOT NULL,PRIMARY KEY(source,start));
                    CREATE VIRTUAL TABLE records_fts USING fts5(text,content='events',content_rowid='rowid');
                    CREATE TRIGGER records_insert AFTER INSERT ON events BEGIN INSERT INTO records_fts(rowid,text) VALUES(new.rowid,new.text);END;
                    CREATE TRIGGER records_delete AFTER DELETE ON events BEGIN INSERT INTO records_fts(records_fts,rowid,text) VALUES('delete',old.rowid,old.text);END;
                    PRAGMA user_version=4;").map_err(|e|e.to_string())?;
            }
            tx.commit().map_err(|e| e.to_string())?;
            sqlite_retry(|| conn.execute_batch("VACUUM;PRAGMA wal_checkpoint(TRUNCATE);"))?;
        }
        conn.execute_batch("CREATE TEMP TABLE selected_sources(path TEXT PRIMARY KEY);")
            .map_err(|e| e.to_string())?;
        Ok(conn)
    }

    #[allow(clippy::too_many_lines)] // One bounded query, its ranking and explicit coverage result.
    pub fn search(&self, request: &SearchRequest) -> Result<SearchResult> {
        validate(request)?;
        let mut plan = query::plan(&request.query, &request.timezone, OffsetDateTime::now_utc())?;
        plan.terms
            .extend(request.terms.iter().map(|s| s.to_lowercase()));
        if request.from.is_some() {
            plan.from.clone_from(&request.from);
        }
        if request.to.is_some() {
            plan.to.clone_from(&request.to);
        }
        let from = plan.from.as_deref().map(query::timestamp).transpose()?;
        let to = plan.to.as_deref().map(query::timestamp).transpose()?;
        if from.zip(to).is_some_and(|(a, b)| a >= b) {
            return Err("from must be earlier than to (exclusive)".into());
        }
        if plan.terms.is_empty()
            && plan.expanded_terms.is_empty()
            && from.is_none()
            && to.is_none()
            && request.session_id.is_none()
            && request.cwd.is_none()
        {
            return Err(
                "provide a natural-language query, terms, time range, session_id or cwd".into(),
            );
        }
        let mut conn = self.connect()?;
        let mut coverage = self.refresh(&mut conn, request.max_index_bytes)?;
        if (from.is_some() || to.is_some()) && coverage.untimed_events > 0 {
            coverage.complete = false;
            warn(&mut coverage, "Events without a parseable timestamp cannot be included in a time-filtered search; search without time bounds to inspect them.".into());
        }
        let fts = query::fts(plan.terms.iter().chain(&plan.expanded_terms).cloned());
        let mut sql = if fts.is_empty() {
            "SELECT e.metadata,e.text,0.0 FROM events e JOIN selected_sources s ON s.path=e.source WHERE 1=1".to_owned()
        } else {
            "SELECT e.metadata,e.text,bm25(records_fts) FROM records_fts JOIN events e ON e.rowid=records_fts.rowid JOIN selected_sources s ON s.path=e.source WHERE records_fts MATCH ?".to_owned()
        };
        let mut args: Vec<rusqlite::types::Value> = Vec::new();
        if !fts.is_empty() {
            args.push(fts.into());
        }
        if let Some(t) = from {
            sql.push_str(" AND e.ts>=?");
            args.push(t.into());
        }
        if let Some(t) = to {
            sql.push_str(" AND e.ts<?");
            args.push(t.into());
        }
        if !request.include_instructions && !request.kinds.iter().any(|k| k == "instruction") {
            sql.push_str(" AND e.kind!='instruction'");
        }
        for (column, value) in [
            ("e.session_id", &request.session_id),
            ("e.cwd", &request.cwd),
        ] {
            if let Some(value) = value {
                let _ = write!(sql, " AND {column}=?");
                args.push(value.clone().into());
            }
        }
        filter_in(&mut sql, &mut args, "e.provider", &request.providers, false);
        filter_in(&mut sql, &mut args, "e.kind", &request.kinds, false);
        filter_in(
            &mut sql,
            &mut args,
            "e.session_id",
            &request.exclude_session_ids,
            true,
        );
        // SQLite ranks candidates, then exact identifiers get priority below.
        sql.push_str(if plan.terms.is_empty() && plan.expanded_terms.is_empty() {
            " ORDER BY e.ts DESC,e.id LIMIT 2001"
        } else {
            " ORDER BY 3,e.ts DESC,e.id LIMIT 2001"
        });
        let mut stmt = conn.prepare(&sql).map_err(|e| e.to_string())?;
        let rows = stmt
            .query_map(params_from_iter(args), |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, f64>(2)?,
                ))
            })
            .map_err(|e| e.to_string())?;
        let mut hits = Vec::new();
        for row in rows {
            let (metadata, search_text, rank) = row.map_err(|e| e.to_string())?;
            let mut event: Event = serde_json::from_str(&metadata).map_err(|e| e.to_string())?;
            let text = std::mem::take(&mut event.text);
            let lower = search_text.to_lowercase();
            let matched_terms: Vec<_> = plan
                .terms
                .iter()
                .chain(&plan.expanded_terms)
                .filter(|t| lower.contains(t.as_str()))
                .cloned()
                .collect();
            let score = -rank
                + plan
                    .terms
                    .iter()
                    .filter(|t| lower.contains(t.as_str()))
                    .map(|t| {
                        if t.contains(['-', '_', '.']) {
                            20.0
                        } else {
                            2.0
                        }
                    })
                    .sum::<f64>();
            let display_lower = text.to_lowercase();
            let anchor = matched_terms
                .iter()
                .filter_map(|t| display_lower.find(t))
                .min()
                .unwrap_or(0);
            let start = text
                .char_indices()
                .map(|(i, _)| i)
                .take_while(|i| *i < anchor.saturating_sub(160))
                .last()
                .unwrap_or(0);
            let (snippet, clipped) = parse::bounded(&text[start..], 900);
            event.text = snippet;
            event.truncated |= clipped || start > 0;
            hits.push(Hit {
                event,
                score,
                matched_terms,
            });
        }
        let candidates_limited = hits.len() > 2000;
        if candidates_limited {
            hits.truncate(2000);
            coverage.warnings.push("Candidate limit reached (2000). Narrow the time range, terms or session; pagination covers these candidates only.".into());
        }
        hits.sort_by(|a, b| {
            b.score
                .total_cmp(&a.score)
                .then_with(|| b.event.timestamp.cmp(&a.event.timestamp))
                .then_with(|| a.event.event_id.cmp(&b.event.event_id))
        });
        let has_more = hits.len() > request.offset + request.limit;
        let matches = hits
            .into_iter()
            .skip(request.offset)
            .take(request.limit)
            .collect();
        Ok(SearchResult { plan, matches, has_more, candidates_limited, next_offset: has_more.then_some(request.offset + request.limit), coverage,
            guidance: vec![
                "Use muxa_agent_read on event_id to inspect surrounding requests, commands and paired results. Prose is not execution evidence.".into(),
                "Broaden with alternate terms or omit keywords and search the event-time window; filter session_id to follow a conversation. Exclude the investigating session to avoid self-matches.".into(),
                "No matches does not prove absence. If coverage.complete=false, repeat to continue indexing and inspect warnings; unsupported/missing sources remain unknown.".into(),
            ] })
    }

    fn refresh(&self, conn: &mut Connection, budget: u64) -> Result<Coverage> {
        let started = Instant::now();
        let mut coverage = Coverage::default();
        let mut visited = HashSet::new();
        let mut files = Vec::new();
        for root in &self.roots {
            if root.exists() {
                coverage.roots.push(root.clone());
                discover(root, &mut visited, &mut files, &mut coverage, started);
            }
        }
        // Least recently attempted first: a continuously growing file must not
        // consume every call's budget before another active file can progress.
        let attempts: std::collections::HashMap<String, i64> = conn
            .prepare("SELECT path,last_attempt FROM sources")
            .map_err(|e| e.to_string())?
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
            .map_err(|e| e.to_string())?
            .collect::<std::result::Result<_, _>>()
            .map_err(|e| e.to_string())?;
        files.sort_by_cached_key(|p| {
            (
                attempts
                    .get(p.to_string_lossy().as_ref())
                    .copied()
                    .unwrap_or(0),
                p.clone(),
            )
        });
        coverage.files_discovered = files.len();
        if files.is_empty() {
            coverage.warnings.push("No transcript files discovered; register additional roots if provider homes are elsewhere.".into());
        }
        // A move into archived_sessions is a new canonical path. Retire vanished
        // sources globally, but retain valid sources outside this query's roots.
        let cached = conn
            .prepare("SELECT path FROM sources")
            .map_err(|e| e.to_string())?
            .query_map([], |r| r.get::<_, String>(0))
            .map_err(|e| e.to_string())?
            .collect::<std::result::Result<Vec<_>, _>>()
            .map_err(|e| e.to_string())?;
        for path in cached {
            if fs::symlink_metadata(&path).is_err_and(|e| e.kind() == std::io::ErrorKind::NotFound)
            {
                conn.execute("DELETE FROM sources WHERE path=?", [path])
                    .map_err(|e| e.to_string())?;
            }
        }
        for path in &files {
            let source = path.to_string_lossy().into_owned();
            conn.execute(
                "INSERT OR IGNORE INTO selected_sources VALUES(?)",
                [&source],
            )
            .map_err(|e| e.to_string())?;
            if coverage.index_bytes >= budget || started.elapsed() >= Duration::from_secs(12) {
                // Cached complete files still count; unfinished ones resume next call.
                if source_current(conn, path).unwrap_or(false) {
                    coverage.files_current += 1;
                }
                continue;
            }
            match index_file(conn, path, budget - coverage.index_bytes, started) {
                Ok(progress) => {
                    coverage.bytes_read += progress.bytes_read;
                    coverage.index_bytes += progress.index_bytes;
                    coverage.files_current += usize::from(progress.current);
                }
                Err(e) => {
                    coverage.files_failed += 1;
                    warn(&mut coverage, format!("{}: {e}", path.display()));
                }
            }
        }
        let (malformed, oversized, unsupported, truncated): (u64,u64,u64,u64) = conn.query_row(
            "SELECT coalesce(sum(malformed),0),coalesce(sum(oversized),0),coalesce(sum(unsupported),0),coalesce(sum(truncated),0) FROM sources JOIN selected_sources USING(path)", [],
            |r| Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?))).map_err(|e| e.to_string())?;
        coverage.malformed_lines = malformed;
        coverage.oversized_lines = oversized;
        coverage.unsupported_records = unsupported;
        coverage.truncated_events = truncated;
        for (count, kind) in [
            (malformed, "malformed lines"),
            (oversized, "oversized lines"),
            (unsupported, "unsupported record/block shapes"),
            (truncated, "truncated event bodies"),
        ] {
            if count > 0 {
                warn(&mut coverage,format!("{count} {kind} leave coverage gaps; repeating an otherwise current index does not remove these gaps."));
            }
        }
        coverage.untimed_events = conn.query_row("SELECT count(*) FROM events JOIN selected_sources ON selected_sources.path=events.source WHERE ts IS NULL", [], |r|r.get(0)).map_err(|e|e.to_string())?;
        coverage.complete = !files.is_empty()
            && coverage.files_current == files.len()
            && coverage.files_failed == 0
            && malformed == 0
            && oversized == 0
            && unsupported == 0
            && truncated == 0;
        if coverage.files_current < files.len() {
            warn(&mut coverage, "Indexing is incomplete (byte/time budget, changed file or unfinished JSONL tail). Repeat this search to continue.".into());
        }
        Ok(coverage)
    }

    #[allow(clippy::too_many_lines)] // Context, paired evidence and output budgets must share the anchor.
    pub fn read(&self, request: &ReadRequest) -> Result<ReadResult> {
        if request.before > 50 || request.after > 50 || !(1..=64_000).contains(&request.max_chars) {
            return Err("before/after must be 0..50 and max_chars 1..64000".into());
        }
        let mut connection = self.connect()?;
        // Keep the anchor, neighboring events and paired results in one read
        // snapshot even while another agent replaces an indexed generation.
        let conn = connection.transaction().map_err(|e| e.to_string())?;
        let anchor = conn
            .query_row(
                "SELECT metadata,text FROM events WHERE id=?",
                [&request.event_id],
                event_row,
            )
            .map_err(|_| {
                "unknown event_id; search first (the index may have been rebuilt)".to_owned()
            })?;
        let mut stmt = conn.prepare("SELECT metadata,text FROM events WHERE source=? AND (line<? OR (line=? AND part<?)) ORDER BY line DESC,part DESC LIMIT ?").map_err(|e| e.to_string())?;
        let mut before = stmt
            .query_map(
                params![
                    anchor.path,
                    anchor.line,
                    anchor.line,
                    anchor.part,
                    request.before + 1
                ],
                event_row,
            )
            .map_err(|e| e.to_string())?
            .collect::<std::result::Result<Vec<_>, _>>()
            .map_err(|e| e.to_string())?;
        let has_before = before.len() > request.before;
        before.truncate(request.before);
        before.reverse();
        let mut stmt = conn.prepare("SELECT metadata,text FROM events WHERE source=? AND (line>? OR (line=? AND part>?)) ORDER BY line,part LIMIT ?").map_err(|e| e.to_string())?;
        let mut after = stmt
            .query_map(
                params![
                    anchor.path,
                    anchor.line,
                    anchor.line,
                    anchor.part,
                    request.after + 1
                ],
                event_row,
            )
            .map_err(|e| e.to_string())?
            .collect::<std::result::Result<Vec<_>, _>>()
            .map_err(|e| e.to_string())?;
        let has_after = after.len() > request.after;
        after.truncate(request.after);
        let mut related = Vec::new();
        let mut related_truncated = false;
        if request.include_tool_pair {
            let mut ids = Vec::new();
            for e in std::iter::once(&anchor).chain(&before).chain(&after) {
                if let Some(id) = &e.call_id {
                    if !ids.contains(id) {
                        ids.push(id.clone());
                    }
                }
            }
            for id in ids {
                // Up to 101 rows may already be the anchor and its neighbors;
                // fetch another 50 plus a sentinel before claiming all pairs fit.
                let mut stmt = conn.prepare("SELECT metadata,text FROM events WHERE provider=? AND session_id=? AND call_id=? AND source=? ORDER BY line,part LIMIT 152").map_err(|e| e.to_string())?;
                for row in stmt
                    .query_map(
                        params![anchor.provider, anchor.session_id, id, anchor.path],
                        event_row,
                    )
                    .map_err(|e| e.to_string())?
                {
                    let e = row.map_err(|e| e.to_string())?;
                    if !Path::new(&e.path).is_file() {
                        continue;
                    }
                    if e.event_id != anchor.event_id
                        && !before
                            .iter()
                            .chain(&after)
                            .any(|v| v.event_id == e.event_id)
                        && !related.iter().any(|v: &Event| v.event_id == e.event_id)
                    {
                        if related.len() < 50 {
                            related.push(e);
                        } else {
                            related_truncated = true;
                        }
                    }
                }
            }
        }
        related.sort_by(|a, b| (&a.path, a.line, a.part).cmp(&(&b.path, b.line, b.part)));
        let source_available = Path::new(&anchor.path).is_file();
        let source_current = source_current(&conn, Path::new(&anchor.path)).unwrap_or(false);
        let mut anchor = anchor;
        let full_len = anchor.text.chars().count();
        anchor.text = anchor.text.chars().skip(request.text_offset).collect();
        // The anchor gets half the budget at minimum, even when neighbors are huge.
        let (text, clipped) =
            parse::bounded(&anchor.text, request.max_chars / 2 + request.max_chars % 2);
        let next_text_offset = clipped.then_some(request.text_offset + text.chars().count());
        anchor.text = text;
        anchor.truncated |= clipped;
        if request.text_offset > full_len {
            return Err("text_offset is beyond the selected event".into());
        }
        let mut remaining = request
            .max_chars
            .saturating_sub(anchor.text.chars().count());
        for e in related
            .iter_mut()
            .chain(before.iter_mut().rev())
            .chain(&mut after)
        {
            let (text, clipped) = parse::bounded(&e.text, remaining.min(4000));
            remaining = remaining.saturating_sub(text.chars().count());
            e.text = text;
            e.truncated |= clipped;
        }
        let mut events = before;
        events.push(anchor);
        events.extend(after);
        Ok(ReadResult { events, related, has_before, has_after, next_text_offset, source_available, source_current, related_truncated,
            guidance: "Cached, redacted evidence. Follow event_id with before/after=0 and text_offset for clipped text. kind=assistant/user is prose, tool_call is an invocation, tool_result is its recorded result; success is not inferred. Missing or unindexed records remain unknown." })
    }
}

fn validate(r: &SearchRequest) -> Result<()> {
    if !(1..=100).contains(&r.limit) || r.offset > 1900 {
        return Err("limit must be 1..100 and offset 0..1900".into());
    }
    if r.query.len() > 8192 || r.terms.len() > 24 || r.terms.iter().any(|t| t.len() > 256) {
        return Err("query/terms too large (8192 query bytes, 24 terms of 256 bytes)".into());
    }
    if r.max_index_bytes == 0 || r.max_index_bytes > 1024 * 1024 * 1024 {
        return Err("max_index_bytes must be 1..1073741824".into());
    }
    if r.providers
        .iter()
        .any(|s| !["claude", "codex"].contains(&s.as_str()))
    {
        return Err("providers must be claude or codex".into());
    }
    if r.kinds.iter().any(|s| {
        ![
            "user",
            "assistant",
            "tool_call",
            "tool_result",
            "instruction",
        ]
        .contains(&s.as_str())
    }) {
        return Err("unknown event kind".into());
    }
    if r.exclude_session_ids.len() > 100 {
        return Err("at most 100 excluded sessions".into());
    }
    Ok(())
}

// SQLite's journal-mode transition can return BUSY without invoking its busy
// handler when several processes initialize the same new cache simultaneously.
fn sqlite_retry<T>(mut operation: impl FnMut() -> rusqlite::Result<T>) -> Result<T> {
    let started = Instant::now();
    loop {
        match operation() {
            Ok(value) => return Ok(value),
            Err(rusqlite::Error::SqliteFailure(error, _))
                if matches!(
                    error.code,
                    rusqlite::ErrorCode::DatabaseBusy | rusqlite::ErrorCode::DatabaseLocked
                ) && started.elapsed() < Duration::from_secs(30) =>
            {
                std::thread::sleep(Duration::from_millis(10));
            }
            Err(error) => return Err(error.to_string()),
        }
    }
}

fn event_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<Event> {
    let metadata: String = row.get(0)?;
    let e: Event = serde_json::from_str(&metadata).map_err(|e| {
        rusqlite::Error::FromSqlConversionFailure(0, rusqlite::types::Type::Text, Box::new(e))
    })?;
    Ok(e)
}

fn filter_in(
    sql: &mut String,
    args: &mut Vec<rusqlite::types::Value>,
    field: &str,
    values: &[String],
    exclude: bool,
) {
    if values.is_empty() {
        return;
    }
    let _ = write!(
        sql,
        " AND {field} {}IN ({})",
        if exclude { "NOT " } else { "" },
        vec!["?"; values.len()].join(",")
    );
    args.extend(values.iter().cloned().map(Into::into));
}

fn warn(c: &mut Coverage, warning: String) {
    if c.warnings.len() < 20 {
        c.warnings.push(warning);
    }
}

fn discover(
    path: &Path,
    seen: &mut HashSet<PathBuf>,
    files: &mut Vec<PathBuf>,
    c: &mut Coverage,
    started: Instant,
) {
    discover_at(path, seen, files, c, started, 0);
}

fn discover_at(
    path: &Path,
    seen: &mut HashSet<PathBuf>,
    files: &mut Vec<PathBuf>,
    c: &mut Coverage,
    started: Instant,
    depth: usize,
) {
    if started.elapsed() >= Duration::from_secs(12)
        || seen.len() >= MAX_DISCOVERY_ENTRIES
        || depth >= 128
    {
        c.files_failed += 1;
        if !c.warnings.iter().any(|w| w.starts_with("Discovery budget")) {
            warn(c,"Discovery budget exceeded (12 seconds / 250000 entries / 128 levels). Narrow roots; coverage is incomplete.".into());
        }
        return;
    }
    let canonical = match fs::canonicalize(path) {
        Ok(p) => p,
        Err(e) => {
            c.files_failed += 1;
            warn(c, format!("{}: {e}", path.display()));
            return;
        }
    };
    if !seen.insert(canonical.clone()) {
        return;
    }
    if canonical.is_file() {
        if canonical.extension().is_some_and(|s| s == "jsonl") {
            files.push(canonical);
        }
    } else if canonical.is_dir() {
        match fs::read_dir(&canonical) {
            Ok(entries) => {
                for entry in entries {
                    match entry {
                        Ok(entry) => {
                            if entry.file_name() == "tool-results"
                                && entry.file_type().is_ok_and(|t| t.is_dir())
                            {
                                continue;
                            }
                            discover_at(&entry.path(), seen, files, c, started, depth + 1);
                            if started.elapsed() >= Duration::from_secs(12)
                                || seen.len() >= MAX_DISCOVERY_ENTRIES
                            {
                                c.files_failed += 1;
                                if !c.warnings.iter().any(|w| w.starts_with("Discovery budget")) {
                                    warn(c, "Discovery budget exceeded (12 seconds / 250000 entries / 128 levels). Narrow roots; coverage is incomplete.".into());
                                }
                                break;
                            }
                        }
                        Err(e) => {
                            c.files_failed += 1;
                            warn(c, e.to_string());
                        }
                    }
                }
            }
            Err(e) => {
                c.files_failed += 1;
                warn(c, format!("{}: {e}", canonical.display()));
            }
        }
    }
}

#[cfg(test)]
mod tests;
