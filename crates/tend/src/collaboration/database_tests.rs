//! Physical write behaviour of the collaboration database.

use super::*;

/// Initializing an up-to-date database must not commit a write merely to
/// validate its schema (it used to rewrite `user_version`).
#[test]
fn reopening_a_current_database_writes_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("collaboration.sqlite3");
    drop(open_database(&path).unwrap());
    let changes = |connection: &rusqlite::Connection| {
        connection
            .query_row("PRAGMA data_version", [], |row| row.get::<_, i64>(0))
            .unwrap()
    };
    let observer = rusqlite::Connection::open(&path).unwrap();
    let before = changes(&observer);
    drop(open_database(&path).unwrap());
    assert_eq!(changes(&observer), before, "reopen committed a write");
}
