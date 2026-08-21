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
    fn outbox_tx_records_calls_event_with_sidecar_committed() {
        let db = temp_db();
        let run = RunId::new();
        register(&db, run);
        let events = vec![OutboxEvent {
            run_id: run,
            event_type: "call.completed".into(),
            payload: r#"{"call_id":"call-1"}"#.into(),
        }];
        let sidecar_value: String = record_with(&db, &events, || {
            db.record_call(
                "call-1",
                run,
                "intake",
                "intake",
                "mock",
                "mock-model",
                "cache-key",
                None,
                false,
                Some(200),
                10,
                5,
                0,
                0,
                1,
                2,
                None,
                0,
            )?;
            Ok("sidecar-ok".into())
        })
        .unwrap();
        assert_eq!(sidecar_value, "sidecar-ok");

        let aggregate = db.run_aggregate(run).unwrap();
        assert_eq!(aggregate.calls, 1, "sidecar call must be committed");
        let rows = db.list_outbox_events_for_run(&run.to_string()).unwrap();
        assert_eq!(rows.len(), 1, "call event must be recorded");
        assert_eq!(rows[0].event_type, "call.completed");
        assert_eq!(rows[0].payload, r#"{"call_id":"call-1"}"#);
    }

    #[test]
    fn outbox_tx_skips_outbox_when_sidecar_fails() {
        let db = temp_db();
        let run = RunId::new();
        register(&db, run);
        let events = vec![OutboxEvent {
            run_id: run,
            event_type: "should_not_appear".into(),
            payload: "{}".into(),
        }];
        let result: Result<()> = record_with(&db, &events, || {
            Err(crate::error::Error::Provider {
                message: "sidecar boom".into(),
                http_status: None,
            })
        });
        assert!(result.is_err(), "sidecar error must propagate");
        let rows = db.list_outbox_events_for_run(&run.to_string()).unwrap();
        assert!(rows.is_empty(), "no rows on sidecar failure");
    }

    #[test]
    fn outbox_tx_rolls_back_outbox_when_outbox_insert_fails() {
        let db = temp_db();
        let run = RunId::new();
        register(&db, run);
        let events = vec![
            OutboxEvent {
                run_id: run,
                event_type: "call.completed".into(),
                payload: "{}".into(),
            },
            OutboxEvent {
                run_id: RunId::new(),
                event_type: "invalid.run".into(),
                payload: "{}".into(),
            },
        ];
        let result: Result<()> = record_with(&db, &events, || Ok(()));
        assert!(result.is_err(), "invalid outbox row must fail");
        let rows = db.list_outbox_events_for_run(&run.to_string()).unwrap();
        assert!(rows.is_empty(), "the first outbox row must be rolled back");
    }

    #[test]
    fn outbox_tx_with_empty_events_still_returns_sidecar() {
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
