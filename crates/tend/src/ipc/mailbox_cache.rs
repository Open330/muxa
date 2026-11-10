//! Compatibility for callers that still expect a full Fleet mailbox each poll.
//! Reuse requires a fresh daemon-validated token response, never a time-to-live.
use crate::collaboration::MailboxSnapshot;
use std::collections::VecDeque;

const MAX_ENTRIES: usize = 32;
const MAX_ENCODED_BYTES: usize = 8 * 1024 * 1024;

#[derive(Debug, Default)]
pub(super) struct MailboxCache {
    rows: VecDeque<(String, MailboxSnapshot, usize)>,
    bytes: usize,
}

impl MailboxCache {
    pub fn get(&mut self, key: &str) -> Option<MailboxSnapshot> {
        let index = self.rows.iter().position(|r| r.0 == key)?;
        let entry = self.rows.remove(index)?;
        let snapshot = entry.1.clone();
        self.rows.push_back(entry);
        Some(snapshot)
    }

    pub fn remove(&mut self, key: &str) {
        if let Some(index) = self.rows.iter().position(|r| r.0 == key) {
            if let Some(entry) = self.rows.remove(index) {
                self.bytes -= entry.2;
            }
        }
    }

    pub fn put(&mut self, key: String, snapshot: &MailboxSnapshot) {
        self.remove(&key);
        if snapshot.token.is_none() || snapshot.unchanged {
            return;
        }
        let Ok(encoded) = serde_json::to_vec(snapshot) else {
            return;
        };
        let size = encoded.len().saturating_add(key.len());
        if size > MAX_ENCODED_BYTES {
            return;
        }
        while self.rows.len() >= MAX_ENTRIES || self.bytes.saturating_add(size) > MAX_ENCODED_BYTES
        {
            let Some(entry) = self.rows.pop_front() else {
                break;
            };
            self.bytes -= entry.2;
        }
        self.bytes += size;
        self.rows.push_back((key, snapshot.clone(), size));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn snapshot() -> MailboxSnapshot {
        MailboxSnapshot {
            token: Some("opaque-revision".into()),
            unchanged: false,
            incoming: Vec::new(),
            sent: Vec::new(),
        }
    }

    #[test]
    fn compatibility_cache_is_lru_bounded_and_never_stores_unsupported_or_unchanged_rows() {
        let mut cache = MailboxCache::default();
        for n in 0..MAX_ENTRIES {
            cache.put(n.to_string(), &snapshot());
        }
        assert!(cache.get("0").is_some());
        cache.put("new".into(), &snapshot());
        assert_eq!(cache.rows.len(), MAX_ENTRIES);
        assert!(cache.get("1").is_none());
        assert!(cache.get("0").is_some());
        let mut unsupported = snapshot();
        unsupported.token = None;
        cache.put("0".into(), &unsupported);
        assert!(cache.get("0").is_none());
        let mut unchanged = snapshot();
        unchanged.unchanged = true;
        cache.put("new".into(), &unchanged);
        assert!(cache.get("new").is_none());
        let mut huge = snapshot();
        huge.token = Some("x".repeat(MAX_ENCODED_BYTES));
        cache.put("too-large".into(), &huge);
        assert!(cache.get("too-large").is_none());
        assert!(cache.bytes <= MAX_ENCODED_BYTES);
    }
}
