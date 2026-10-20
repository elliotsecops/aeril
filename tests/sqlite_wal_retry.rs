//! Regression test for concurrent WAL-mode initialization in Store::open.

use aeril::error::Error;
use aeril::store::Store;
use rusqlite::{Connection, ErrorCode};
use std::time::{Duration, Instant};

#[test]
fn wal_mode_busy_retry_has_bounded_deadline_and_does_not_migrate() {
    let dir = std::env::temp_dir().join(format!("aeril-wal-retry-{}", aeril::ids::new_id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("aeril.db");

    // Hold the rollback-journal DB exclusively so Store::open cannot switch it
    // to WAL until the bounded retry deadline expires.
    let holder = Connection::open(&path).unwrap();
    holder.execute_batch("BEGIN EXCLUSIVE;").unwrap();

    let started = Instant::now();
    let error = match Store::open(&path) {
        Ok(_) => panic!("exclusive holder must block WAL transition"),
        Err(error) => error,
    };
    let elapsed = started.elapsed();
    println!("WAL_RETRY_DEADLINE elapsed_ms={}", elapsed.as_millis());

    let sqlite_code = match &error {
        Error::Store(sqlite_error) => sqlite_error.sqlite_error_code(),
        _ => None,
    };
    assert_eq!(
        sqlite_code,
        Some(ErrorCode::DatabaseBusy),
        "error={error:?}"
    );
    assert!(
        elapsed >= Duration::from_millis(4_500),
        "retry stopped before the existing 5-second deadline: {} ms",
        elapsed.as_millis()
    );
    assert!(
        elapsed < Duration::from_millis(6_500),
        "retry exceeded its bounded deadline: {} ms",
        elapsed.as_millis()
    );

    holder.execute_batch("ROLLBACK;").unwrap();
    let version: i64 = holder
        .query_row("PRAGMA user_version", [], |row| row.get(0))
        .unwrap();
    assert_eq!(version, 0, "failed WAL transition must not run migration");
    let mode: String = holder
        .query_row("PRAGMA journal_mode", [], |row| row.get(0))
        .unwrap();
    assert_eq!(
        mode, "delete",
        "failed conversion leaves previous mode intact"
    );

    drop(holder);
    std::fs::remove_dir_all(dir).unwrap();
}
