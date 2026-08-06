//! D.1.4: outbox transaccional real.
//!
//! Wraps the caller's "sidecar write + outbox insert" pattern in a
//! SQL transaction so the two operations either both succeed or
//! neither succeeds. Without this, a crash between the sidecar
//! write and the `outbox_events` INSERT leaves the run with a
//! sidecar the dispatcher will never see and no row to recover
//! from; with this, the run is durable end-to-end.
//!
//! [`record_with`] runs the caller's sidecar work first, then opens
//! a single transaction, inserts every event, and commits. If the
//! transaction fails the error propagates and the caller is
//! expected to either roll back the sidecar or rely on the
//! dispatcher's reconcile step (D.28.1) to spot the missing row on
//! the next scan.

use crate::error::Result;
use crate::ids::RunId;
use crate::storage::sqlite::Db;

/// Single outbox row to be inserted atomically with the caller's
/// sidecar work. The `run_id`, `event_type` and `payload` map
/// directly to the `outbox_events` table columns (D.1.4). `at_unix`
/// is set inside the SQL statement to `strftime('%s','now')` so the
/// timestamp is monotonic across the whole batch.
pub struct OutboxEvent {
    /// Run the event belongs to.
    pub run_id: RunId,
    /// Event type tag (free-form, e.g. `"sidecar_written"`).
    pub event_type: String,
    /// Payload (free-form JSON or text).
    pub payload: String,
}

/// Run `sidecar_write`, then atomically insert every event in
/// `events` into `outbox_events` inside a single SQLite
/// transaction. Returns the value `sidecar_write` produced.
///
/// Order matters: the sidecar is written first, so a successful
/// return value but failed transaction leaves a sidecar with no
/// matching outbox row — the dispatcher will see the sidecar on
/// the next reconcile pass (D.28.1). A failed sidecar short-circuits
/// the whole call: no transaction is opened, no events are
/// recorded, and the sidecar error is propagated untouched.
pub fn record_with<T, F>(db: &Db, events: &[OutboxEvent], sidecar_write: F) -> Result<T>
where
    F: FnOnce() -> Result<T>,
{
    let sidecar_result = sidecar_write()?;
    let conn = db.pool().get()?;
    let tx = conn.unchecked_transaction()?;
    for ev in events {
        tx.execute(
            "INSERT INTO outbox_events (run_id, event_type, payload, at_unix) \
             VALUES (?, ?, ?, strftime('%s','now'))",
            rusqlite::params![ev.run_id.to_string(), ev.event_type, ev.payload],
        )?;
    }
    tx.commit()?;
    Ok(sidecar_result)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::sqlite::Db;

    fn temp_db() -> Db {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("outbox.sqlite");
        std::mem::forget(tmp);
        Db::open(&path).unwrap()
    }

    fn register(db: &Db, id: RunId) {
        db.register_run(id, "fast", "running", "0.1.0", None, None, None)
            .unwrap();
    }

    #[test]
    fn outbox_tx_records_all_events_in_transaction() {
        let db = temp_db();
        let run = RunId::new();
        register(&db, run);
        let events = vec![
            OutboxEvent {
                run_id: run,
                event_type: "sidecar_written".into(),
                payload: r#"{"path":"a.json"}"#.into(),
            },
            OutboxEvent {
                run_id: run,
                event_type: "sidecar_written".into(),
                payload: r#"{"path":"b.json"}"#.into(),
            },
            OutboxEvent {
                run_id: run,
                event_type: "phase_end".into(),
                payload: r#"{"phase":"intake"}"#.into(),
            },
        ];
        let sidecar_value: String = record_with(&db, &events, || Ok("sidecar-ok".into())).unwrap();
        assert_eq!(sidecar_value, "sidecar-ok");

        let rows = db.list_outbox_events_for_run(&run.to_string()).unwrap();
        assert_eq!(rows.len(), 3, "all events must be recorded");
        assert_eq!(rows[0].event_type, "sidecar_written");
        assert_eq!(rows[1].event_type, "sidecar_written");
        assert_eq!(rows[2].event_type, "phase_end");
        assert!(
            rows.iter().all(|r| r.payload.contains('{')),
            "payload preserved"
        );
    }

    #[test]
    fn outbox_tx_propagates_sidecar_failure_without_inserting() {
        let db = temp_db();
        let run = RunId::new();
        register(&db, run);
        let events = vec![OutboxEvent {
            run_id: run,
            event_type: "should_not_appear".into(),
            payload: "{}".into(),
        }];
        let result: Result<()> = record_with(&db, &events, || {
            Err(crate::error::Error::Provider("sidecar boom".into()))
        });
        assert!(result.is_err(), "sidecar error must propagate");
        let rows = db.list_outbox_events_for_run(&run.to_string()).unwrap();
        assert!(rows.is_empty(), "no rows on sidecar failure");
    }

    #[test]
    fn outbox_tx_empty_events_still_returns_sidecar_value() {
        let db = temp_db();
        let run = RunId::new();
        register(&db, run);
        let events: Vec<OutboxEvent> = Vec::new();
        let v: u32 = record_with(&db, &events, || Ok(42)).unwrap();
        assert_eq!(v, 42);
        let rows = db.list_outbox_events_for_run(&run.to_string()).unwrap();
        assert!(rows.is_empty());
    }
}
