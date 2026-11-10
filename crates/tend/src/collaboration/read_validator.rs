//! Small validator memo: no result rows, per-client state, or expiry policy.
use serde::Serialize;
use sha2::{Digest, Sha256};
use std::{collections::VecDeque, io};

type Key = [u8; 32];
type Revision = (u64, u64);
const MAX_ENTRIES: usize = 128;

#[derive(Debug, Default)]
pub(super) struct ReadValidators {
    entries: VecDeque<(Key, Revision, String)>,
}

impl ReadValidators {
    pub fn matches(&mut self, key: Key, revision: Revision, token: &str) -> bool {
        let Some(index) = self.entries.iter().position(|entry| entry.0 == key) else {
            return false;
        };
        let entry = self.entries.remove(index).expect("located entry");
        let matches = entry.1 == revision && entry.2 == token;
        self.entries.push_back(entry);
        matches
    }

    pub fn remember(&mut self, key: Key, revision: Revision, token: String) {
        self.entries.retain(|entry| entry.0 != key);
        if self.entries.len() == MAX_ENTRIES {
            self.entries.pop_front();
        }
        self.entries.push_back((key, revision, token));
    }
}

/// Hash the actual serialized representation without allocating/copying its bodies.
pub(super) fn fingerprint(value: &impl Serialize) -> Result<Key, serde_json::Error> {
    struct Writer(Sha256);
    impl io::Write for Writer {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            self.0.update(bytes);
            Ok(bytes.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }
    let mut writer = Writer(Sha256::new());
    serde_json::to_writer(&mut writer, value)?;
    Ok(writer.0.finalize().into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validator_memo_is_bounded_and_eviction_only_loses_a_fast_path() {
        let mut cache = ReadValidators::default();
        for n in 0..MAX_ENTRIES {
            cache.remember(fingerprint(&n).unwrap(), (1, 2), n.to_string());
        }
        assert!(cache.matches(fingerprint(&0).unwrap(), (1, 2), "0"));
        cache.remember(fingerprint(&MAX_ENTRIES).unwrap(), (1, 2), "new".into());
        assert_eq!(cache.entries.len(), MAX_ENTRIES);
        assert!(!cache.matches(fingerprint(&1).unwrap(), (1, 2), "1"));
        assert!(cache.matches(fingerprint(&0).unwrap(), (1, 2), "0"));
        assert!(!cache.matches(fingerprint(&0).unwrap(), (2, 2), "0"));
        assert!(!cache.matches(fingerprint(&0).unwrap(), (1, 2), "unissued"));
    }
}
