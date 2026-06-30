//! SQLite-backed persistent memory — see v3 §3.2.0 + AC-V3-3.6.
//!
//! Wraps `rusqlite` to persist the [`Memory`] (Phase 3.x) across
//! daemon restarts. The on-disk schema is intentionally simple:
//!
//! ```sql
//! CREATE TABLE screens (
//!     screen_id BLOB PRIMARY KEY,           -- 16-byte ScreenId
//!     successes BLOB NOT NULL,             -- postcard-encoded `Vec<Action>`
//!     failures  BLOB NOT NULL              -- postcard-encoded `Vec<(Action, String)>`
//! );
//! ```
//!
//! Optional: compiled only when the `sqlite` feature is enabled
//! (default-on for `ai-device-kernel`; off for minimal binaries).

#![cfg(feature = "sqlite")]

use std::path::Path;

use rusqlite::{params, Connection};

use crate::action::Action;
use crate::ids::ScreenId;
use crate::memory::{ActionSequence, Memory};

/// Open or create a SQLite-backed memory at `path`. The
/// returned [`Memory`] starts empty but is hydrated from any
/// rows already present in the file.
#[cfg(feature = "sqlite")]
pub fn open(path: impl AsRef<Path>) -> rusqlite::Result<(Connection, Memory)> {
    let conn = Connection::open(path)?;
    // Schema bootstrap.
    conn.execute_batch(
        r#"
        CREATE TABLE IF NOT EXISTS screens (
            screen_id BLOB PRIMARY KEY,
            successes BLOB NOT NULL,
            failures  BLOB NOT NULL
        );
        "#,
    )?;
    // Hydrate in-memory cache. We collect the rows eagerly
    // (using `next()` to drain the iterator) so the
    // `Statement` borrow is released before we re-borrow
    // `conn` for the migration.
    let mut mem = Memory::new();
    {
        let mut stmt = conn
            .prepare("SELECT screen_id, successes, failures FROM screens")?;
        let mut rows = stmt.query([])?;
        while let Some(row) = rows.next()? {
            let screen_id_bytes: Vec<u8> = row.get(0)?;
            let mut id = [0u8; 16];
            if screen_id_bytes.len() == 16 {
                id.copy_from_slice(&screen_id_bytes);
            }
            let successes_bytes: Vec<u8> = row.get(1)?;
            let failures_bytes: Vec<u8> = row.get(2)?;
            let successes: Vec<Action> = postcard::from_bytes(&successes_bytes).unwrap_or_default();
            let failures: Vec<(Action, String)> =
                postcard::from_bytes(&failures_bytes).unwrap_or_default();
            let sid = ScreenId(id);
            for s in successes {
                mem.record_success(sid, s);
            }
            for (a, reason) in failures {
                mem.record_failure(sid, a, reason);
            }
        }
    }
    Ok((conn, mem))
}

/// Persist a single screen entry. Idempotent on the primary
/// key (`screen_id`); overwrites the row if it already exists.
#[cfg(feature = "sqlite")]
pub fn persist_screen(
    conn: &Connection,
    sid: ScreenId,
    entry: &ActionSequence,
) -> rusqlite::Result<()> {
    let successes_bytes = postcard::to_allocvec(&entry.successes)
        .map_err(|e| rusqlite::Error::ToSqlConversionFailure(Box::new(e)))?;
    let failures_bytes = postcard::to_allocvec(&entry.failures)
        .map_err(|e| rusqlite::Error::ToSqlConversionFailure(Box::new(e)))?;
    conn.execute(
        "INSERT OR REPLACE INTO screens (screen_id, successes, failures) \
         VALUES (?1, ?2, ?3)",
        params![&sid.0[..], successes_bytes, failures_bytes],
    )?;
    Ok(())
}

#[cfg(all(test, feature = "sqlite"))]
mod tests {
    use super::*;

    fn tmp_path(label: &str) -> std::path::PathBuf {
        let mut p = std::env::temp_dir();
        p.push(format!(
            "adk-mem-{}-{}-{}.sqlite",
            label,
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        let _ = std::fs::remove_file(&p);
        p
    }

    #[test]
    fn open_persists_records_across_restart() {
        let path = tmp_path("round-trip");
        let sid = ScreenId::compute(b"a11y-A", b"ph-A", "com.foo/.A");
        // Round 1: write.
        {
            let (conn, mut mem) = open(&path).expect("open");
            mem.record_success(sid, Action::Tap {
                x: 1,
                y: 2,
                deadline_ms: 1,
            });
            mem.record_success(sid, Action::Tap {
                x: 3,
                y: 4,
                deadline_ms: 1,
            });
            let entry = mem.peek(sid).expect("present after write");
            persist_screen(&conn, sid, entry).expect("persist");
            conn.close().ok();
        }
        // Round 2: reopen → re-hydrated.
        {
            let (_conn, mut mem) = open(&path).expect("reopen");
            assert!(mem.lookup(sid).is_some(), "screen entry survives");
            let entry = mem.peek(sid).expect("present after reopen");
            assert_eq!(entry.successes.len(), 2, "two actions replayed");
            let _ = std::fs::remove_file(&path);
        }
    }

    #[test]
    fn open_handles_empty_file() {
        let path = tmp_path("empty");
        let (_conn, mem) = open(&path).expect("open");
        assert!(mem.is_empty());
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn hydrate_many_screens_then_persist_one() {
        let path = tmp_path("many");
        let (_conn, mut mem) = open(&path).expect("open");
        for i in 0..5 {
            let sid = ScreenId::compute(
                format!("a11y-{i}").as_bytes(),
                format!("ph-{i}").as_bytes(),
                &format!("com.foo/.S{i}"),
            );
            mem.record_success(sid, Action::Tap {
                x: i,
                y: i,
                deadline_ms: 1,
            });
        }
        assert_eq!(mem.len(), 5);
        let _ = std::fs::remove_file(&path);
    }
}
