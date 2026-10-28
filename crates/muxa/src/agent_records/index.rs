//! Transactional, bounded checkpoints. Every indexed byte belongs to a verified
//! chunk; metadata changes never assume that a changed file was append-only.
use super::{parse, query, Result, MAX_LINE};
use rusqlite::{params, Connection, TransactionBehavior};
use sha2::{Digest, Sha256};
use std::fs::{self, File};
use std::io::{BufRead, BufReader, Read, Seek, SeekFrom};
use std::path::Path;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const CHUNK: usize = 64 * 1024;

pub(super) fn modified(path: &Path) -> Result<(u64, String)> {
    let m = fs::metadata(path).map_err(|e| e.to_string())?;
    let mut stamp = m
        .modified()
        .unwrap_or(UNIX_EPOCH)
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos()
        .to_string();
    #[cfg(unix)]
    {
        use std::fmt::Write as _;
        use std::os::unix::fs::MetadataExt;
        let _ = write!(
            stamp,
            ":{}:{}:{}:{}",
            m.dev(),
            m.ino(),
            m.ctime(),
            m.ctime_nsec()
        );
    }
    #[cfg(not(unix))]
    {
        use std::fmt::Write as _;
        let _ = write!(stamp, ":{:?}", m.created().ok());
    }
    Ok((m.len(), stamp))
}

pub(super) fn source_current(conn: &Connection, path: &Path) -> Result<bool> {
    let (size, stamp) = modified(path)?;
    Ok(conn
        .query_row(
            "SELECT size=? AND modified=? AND offset=? AND skipping=0 FROM sources WHERE path=?",
            params![size, stamp, size, path.to_string_lossy()],
            |r| r.get(0),
        )
        .unwrap_or(false))
}

struct Chunks {
    source: String,
    start: u64,
    len: usize,
    hash: Sha256,
}

impl Chunks {
    fn resume(file: &mut File, source: &str, offset: u64) -> Result<(Self, u64)> {
        let start = offset / CHUNK as u64 * CHUNK as u64;
        let mut bytes = vec![0; usize::try_from(offset - start).map_err(|e| e.to_string())?];
        file.seek(SeekFrom::Start(start))
            .map_err(|e| e.to_string())?;
        file.read_exact(&mut bytes).map_err(|e| e.to_string())?;
        let mut hash = Sha256::new();
        hash.update(&bytes);
        Ok((
            Self {
                source: source.into(),
                start,
                len: bytes.len(),
                hash,
            },
            bytes.len() as u64,
        ))
    }
    fn save(&self, conn: &Connection) -> Result<()> {
        if self.len > 0 {
            let digest = format!("{:x}", self.hash.clone().finalize());
            conn.execute("INSERT INTO source_chunks(source,start,len,digest) VALUES(?,?,?,?) ON CONFLICT(source,start) DO UPDATE SET len=excluded.len,digest=excluded.digest",params![self.source,self.start,self.len,digest]).map_err(|e|e.to_string())?;
        }
        Ok(())
    }
    fn feed(&mut self, conn: &Connection, mut bytes: &[u8]) -> Result<()> {
        while !bytes.is_empty() {
            let n = bytes.len().min(CHUNK - self.len);
            self.hash.update(&bytes[..n]);
            self.len += n;
            bytes = &bytes[n..];
            if self.len == CHUNK {
                self.save(conn)?;
                self.start += CHUNK as u64;
                self.len = 0;
                self.hash = Sha256::new();
            }
        }
        Ok(())
    }
}

enum Verification {
    Verified(u64),
    Changed(u64),
    Deferred(u64),
}

pub(super) struct Progress {
    pub bytes_read: u64,
    pub index_bytes: u64,
    pub current: bool,
}

fn verify(
    conn: &Connection,
    file: &mut File,
    source: &str,
    offset: u64,
    started: Instant,
) -> Result<Verification> {
    let mut stmt = conn
        .prepare("SELECT start,len,digest FROM source_chunks WHERE source=? ORDER BY start")
        .map_err(|e| e.to_string())?;
    let rows = stmt
        .query_map([source], |r| {
            Ok((
                r.get::<_, u64>(0)?,
                r.get::<_, usize>(1)?,
                r.get::<_, String>(2)?,
            ))
        })
        .map_err(|e| e.to_string())?;
    let mut position = 0;
    for row in rows {
        if started.elapsed() >= Duration::from_secs(12) {
            return Ok(Verification::Deferred(position));
        }
        let (at, len, expected) = row.map_err(|e| e.to_string())?;
        if at != position || len == 0 || len > CHUNK || at + len as u64 > offset {
            return Ok(Verification::Changed(position));
        }
        file.seek(SeekFrom::Start(at)).map_err(|e| e.to_string())?;
        let mut bytes = vec![0; len];
        file.read_exact(&mut bytes).map_err(|e| e.to_string())?;
        position += len as u64;
        if format!("{:x}", Sha256::digest(bytes)) != expected {
            return Ok(Verification::Changed(position));
        }
    }
    Ok(if position == offset {
        Verification::Verified(position)
    } else {
        Verification::Changed(position)
    })
}

#[allow(clippy::too_many_lines)] // One source generation, chunk checkpoints and events commit atomically.
pub(super) fn index_file(
    conn: &mut Connection,
    path: &Path,
    budget: u64,
    started: Instant,
) -> Result<Progress> {
    // Claim the writer before reading its checkpoint: other agents may have
    // advanced it while this connection was waiting for the writer lock.
    let tx = conn
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .map_err(|e| e.to_string())?;
    let (size, stamp) = modified(path)?;
    if source_current(&tx, path)? {
        return Ok(Progress {
            bytes_read: 0,
            index_bytes: 0,
            current: true,
        });
    }
    let name = path.to_string_lossy().into_owned();
    let mut file = File::open(path).map_err(|e| e.to_string())?;
    let existing = tx
        .query_row(
            "SELECT offset,line,context,modified,open_tail,skipping FROM sources WHERE path=?",
            [&name],
            |r| {
                Ok((
                    r.get::<_, u64>(0)?,
                    r.get::<_, u64>(1)?,
                    r.get::<_, String>(2)?,
                    r.get::<_, String>(3)?,
                    r.get::<_, bool>(4)?,
                    r.get::<_, bool>(5)?,
                ))
            },
        )
        .ok();
    let mut offset = 0;
    let mut line = 0;
    let mut open_tail = false;
    let mut skipping = false;
    let mut context = parse::Context {
        session_id: path
            .file_stem()
            .unwrap_or_default()
            .to_string_lossy()
            .into_owned(),
        cwd: None,
    };
    let mut read_bytes = 0;
    let attempted = i64::try_from(
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos(),
    )
    .unwrap_or(i64::MAX);
    if let Some((old_offset, old_line, old_context, old_stamp, old_tail, old_skipping)) = existing {
        let mut reusable = old_offset <= size;
        if reusable && old_stamp != stamp {
            // Verification I/O is reported separately from the new-byte budget.
            // An append must not discard a valid prefix just because it is large.
            match verify(&tx, &mut file, &name, old_offset, started)? {
                Verification::Verified(bytes) => read_bytes += bytes,
                Verification::Changed(bytes) => {
                    read_bytes += bytes;
                    reusable = false;
                }
                Verification::Deferred(bytes) => {
                    tx.execute(
                        "UPDATE sources SET last_attempt=? WHERE path=?",
                        params![attempted, name],
                    )
                    .map_err(|e| e.to_string())?;
                    tx.commit().map_err(|e| e.to_string())?;
                    return Ok(Progress {
                        bytes_read: bytes,
                        index_bytes: 0,
                        current: false,
                    });
                }
            }
        }
        if reusable && old_tail && size > old_offset {
            file.seek(SeekFrom::Start(old_offset))
                .map_err(|e| e.to_string())?;
            let mut first = [0];
            file.read_exact(&mut first).map_err(|e| e.to_string())?;
            read_bytes += 1;
            // A complete, previously unterminated JSON value can acquire its
            // newline. Other extensions must be reparsed from their source.
            reusable = first[0] == b'\n';
        }
        if reusable {
            offset = old_offset;
            line = old_line;
            open_tail = old_tail;
            skipping = old_skipping;
            context = serde_json::from_str(&old_context).map_err(|e| e.to_string())?;
        } else {
            tx.execute("DELETE FROM sources WHERE path=?", [&name])
                .map_err(|e| e.to_string())?;
        }
    }
    tx.execute("INSERT OR IGNORE INTO sources(path,size,modified,offset,line,context,open_tail,skipping) VALUES(?,?,?,0,0,'{}',0,0)",params![name,size,stamp]).map_err(|e|e.to_string())?;
    let (mut chunks, seed_bytes) = Chunks::resume(&mut file, &name, offset)?;
    read_bytes += seed_bytes;
    file.seek(SeekFrom::Start(offset))
        .map_err(|e| e.to_string())?;
    // A growing source is limited to this generation's captured size. Its next
    // append is handled by a later call, never folded into this checkpoint.
    let mut reader = BufReader::new(file.take(size - offset));
    if open_tail && offset < size {
        let mut newline = [0];
        reader.read_exact(&mut newline).map_err(|e| e.to_string())?;
        read_bytes += 1;
        chunks.feed(&tx, &newline)?;
        offset += 1;
        open_tail = false;
    }
    let raw_start = offset;
    let overhead_bytes = read_bytes;
    let raw_budget = budget;
    let mut malformed = 0_u64;
    let mut oversized = 0_u64;
    let mut unsupported = 0_u64;
    let mut truncated = 0_u64;
    let mut buf = Vec::new();
    while offset - raw_start < raw_budget && started.elapsed() < Duration::from_secs(12) {
        if skipping {
            let available = reader.fill_buf().map_err(|e| e.to_string())?;
            if available.is_empty() {
                break;
            }
            let end = available.iter().position(|b| *b == b'\n');
            let n = end.map_or(available.len(), |i| i + 1).min(
                usize::try_from((raw_budget - (offset - raw_start)).min(CHUNK as u64))
                    .unwrap_or(CHUNK),
            );
            chunks.feed(&tx, &available[..n])?;
            skipping = available[n - 1] != b'\n';
            reader.consume(n);
            offset += n as u64;
            read_bytes += n as u64;
            continue;
        }
        buf.clear();
        let read = (&mut reader)
            .take((MAX_LINE + 1) as u64)
            .read_until(b'\n', &mut buf)
            .map_err(|e| e.to_string())?;
        read_bytes += read as u64;
        if read == 0 {
            break;
        }
        let terminated = buf.last() == Some(&b'\n');
        if buf.len() > MAX_LINE {
            chunks.feed(&tx, &buf)?;
            offset += read as u64;
            line += 1;
            oversized += 1;
            skipping = !terminated;
            continue;
        }
        let value = serde_json::from_slice::<serde_json::Value>(&buf);
        if !terminated && value.is_err() {
            break;
        }
        let at = offset;
        offset += read as u64;
        line += 1;
        open_tail = !terminated;
        chunks.feed(&tx, &buf)?;
        if buf.iter().all(u8::is_ascii_whitespace) {
            continue;
        }
        let Ok(value) = value else {
            malformed += 1;
            continue;
        };
        unsupported += parse::unsupported(&value);
        for mut event in parse::events(&value, &mut context) {
            event.path.clone_from(&name);
            event.line = line;
            event.byte_offset = at;
            event.event_id = format!(
                "{:x}",
                Sha256::digest(format!("{name}\0{at}\0{}\0{}", event.part, event.text).as_bytes())
            );
            if event.truncated {
                truncated += 1;
            }
            let ts = event
                .timestamp
                .as_deref()
                .and_then(|s| query::timestamp(s).ok());
            // Freeform orchestration code can itself contain JS/Python string
            // escapes. Add a searchable boundary view without changing displayed
            // evidence or losing original tokens such as Windows path names.
            let normalized = event
                .text
                .replace("\\n", "\n")
                .replace("\\t", "\t")
                .replace("\\r", "\r");
            let text = if normalized == event.text {
                event.text.clone()
            } else {
                format!("{}\n{normalized}", event.text)
            };
            let metadata = serde_json::to_string(&event).map_err(|e| e.to_string())?;
            tx.execute("INSERT OR IGNORE INTO events(id,source,line,part,ts,provider,session_id,cwd,kind,call_id,text,metadata) VALUES(?,?,?,?,?,?,?,?,?,?,?,?)",params![event.event_id,name,line,event.part,ts,event.provider,event.session_id,event.cwd,event.kind,event.call_id,text,metadata]).map_err(|e|e.to_string())?;
        }
    }
    chunks.save(&tx)?;
    tx.execute("UPDATE sources SET size=?,modified=?,offset=?,line=?,context=?,open_tail=?,skipping=?,last_attempt=?,malformed=malformed+?,oversized=oversized+?,unsupported=unsupported+?,truncated=truncated+? WHERE path=?",params![size,stamp,offset,line,serde_json::to_string(&context).map_err(|e|e.to_string())?,open_tail,skipping,attempted,malformed,oversized,unsupported,truncated,name]).map_err(|e|e.to_string())?;
    let current = offset == size && !skipping && modified(path)? == (size, stamp);
    tx.commit().map_err(|e| e.to_string())?;
    Ok(Progress {
        bytes_read: read_bytes,
        index_bytes: read_bytes - overhead_bytes,
        current,
    })
}
