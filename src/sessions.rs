//! Exact native session bindings for the experimental provider adapters. The
//! provider, project and task cannot silently change after first use.
use rusqlite::{Connection, OptionalExtension, Transaction, params};

use crate::store::{Result, StoreError};

pub const SCHEMA: &str = "
CREATE TABLE agent_session_binding (
    provider TEXT NOT NULL CHECK(provider IN ('codex','claude')),
    native_session TEXT NOT NULL,
    item_id INTEGER NOT NULL REFERENCES item(id),
    workspace TEXT NOT NULL,
    PRIMARY KEY(provider,native_session)
) STRICT;
CREATE TABLE agent_session_run (
    run_id INTEGER PRIMARY KEY REFERENCES agent_run(id),
    workspace TEXT NOT NULL,
    requested_session TEXT,
    native_session TEXT
) STRICT;
";

#[derive(Clone, Copy, Debug)]
pub struct Request<'a> {
    pub workspace: &'a str,
    pub expected: Option<&'a str>,
}

/// Accept only a canonical lowercase UUID. Never pass a display name, path,
/// sentinel, workspace selector or command-line flag to a CLI as a session.
pub fn valid_id(id: &str) -> bool {
    id.len() == 36
        && id.bytes().enumerate().all(|(idx, b)| {
            if matches!(idx, 8 | 13 | 18 | 23) {
                b == b'-'
            } else {
                b.is_ascii_digit() || (b'a'..=b'f').contains(&b)
            }
        })
        && id != "00000000-0000-0000-0000-000000000000"
}

fn scope(provider: &str, request: Request<'_>) -> Result<()> {
    if !matches!(provider, "codex" | "claude") || request.workspace.is_empty() {
        return Err(StoreError::Invalid("invalid native session scope"));
    }
    if request.expected.is_some_and(|id| !valid_id(id)) {
        return Err(StoreError::Invalid(
            "native session id must be a canonical lowercase UUID",
        ));
    }
    Ok(())
}

pub fn check(c: &Connection, item: i64, provider: &str, request: Request<'_>) -> Result<()> {
    scope(provider, request)?;
    if let Some(id) = request.expected
        && let Some((bound_item, workspace)) = binding(c, provider, id)?
        && (bound_item != item || workspace != request.workspace)
    {
        return Err(StoreError::Invalid(
            "native session belongs to another task or workspace",
        ));
    }
    Ok(())
}

fn binding(c: &Connection, provider: &str, id: &str) -> Result<Option<(i64, String)>> {
    Ok(c.query_row(
        "SELECT item_id,workspace FROM agent_session_binding WHERE provider=? AND native_session=?",
        params![provider, id],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )
    .optional()?)
}

fn bind(tx: &Transaction<'_>, item: i64, provider: &str, workspace: &str, id: &str) -> Result<()> {
    check(
        tx,
        item,
        provider,
        Request {
            workspace,
            expected: Some(id),
        },
    )?;
    tx.execute(
        "INSERT INTO agent_session_binding(provider,native_session,item_id,workspace)
         VALUES(?,?,?,?) ON CONFLICT(provider,native_session) DO NOTHING",
        params![provider, id, item, workspace],
    )?;
    Ok(())
}

/// Called inside the same immediate transaction that starts the run. Adopting
/// a supplied ID reserves it before the process can access the native session.
pub fn reserve(
    tx: &Transaction<'_>,
    run: i64,
    item: i64,
    provider: &str,
    request: Request<'_>,
) -> Result<()> {
    check(tx, item, provider, request)?;
    if let Some(id) = request.expected {
        bind(tx, item, provider, request.workspace, id)?;
    }
    tx.execute(
        "INSERT INTO agent_session_run(run_id,workspace,requested_session) VALUES(?,?,?)",
        params![run, request.workspace, request.expected],
    )?;
    Ok(())
}

/// A successful persistent run must report a valid ID. A resume must report
/// exactly the ID chosen before dispatch; never silently bind a replacement.
pub fn finish(
    tx: &Transaction<'_>,
    run: i64,
    state: &str,
    observed_id: Option<&str>,
) -> Result<()> {
    let row: Option<(i64, String, String, Option<String>)> = tx
        .query_row(
            "SELECT a.item_id,a.provider,s.workspace,s.requested_session
             FROM agent_session_run s JOIN agent_run a ON a.id=s.run_id WHERE s.run_id=?",
            [run],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
        )
        .optional()?;
    match (row, state, observed_id) {
        (None, _, None) | (Some(_), "failed", None) => Ok(()),
        (Some((item, provider, workspace, requested)), "review", Some(id))
            if valid_id(id) && requested.as_deref().is_none_or(|wanted| wanted == id) =>
        {
            bind(tx, item, &provider, &workspace, id)?;
            tx.execute(
                "UPDATE agent_session_run SET native_session=? WHERE run_id=?",
                params![id, run],
            )?;
            Ok(())
        }
        _ => Err(StoreError::Invalid(
            "provider did not confirm the chosen native session",
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const ID: &str = "01892af6-8270-7ced-86c1-929b50dadcb5";
    const OTHER: &str = "361bf867-7c96-4097-9b3f-01cc00f12629";

    #[test]
    fn validates_only_canonical_uuids() {
        assert!(valid_id(ID));
        for invalid in [
            "last",
            "--last",
            "thread-12",
            "/tmp/session.jsonl",
            "01892AF6-8270-7CED-86C1-929B50DADCB5",
            "00000000-0000-0000-0000-000000000000",
            "01892af6-8270-7ced-86c1-929b50dadcb5\n",
        ] {
            assert!(!valid_id(invalid), "{invalid:?}");
        }
    }

    #[test]
    fn exact_session_cannot_switch_task_workspace_or_result_id() {
        let mut c = Connection::open_in_memory().unwrap();
        c.execute_batch(
            "PRAGMA foreign_keys=ON;
             CREATE TABLE item(id INTEGER PRIMARY KEY);
             CREATE TABLE agent_run(id INTEGER PRIMARY KEY,item_id INTEGER,provider TEXT);
             INSERT INTO item VALUES(1),(2);
             INSERT INTO agent_run VALUES(1,1,'codex'),(2,1,'codex'),(3,2,'codex'),(4,2,'claude');",
        )
        .unwrap();
        c.execute_batch(SCHEMA).unwrap();
        let tx = c.transaction().unwrap();
        let new = Request {
            workspace: "/trusted/one",
            expected: None,
        };
        reserve(&tx, 1, 1, "codex", new).unwrap();
        assert!(finish(&tx, 1, "review", None).is_err());
        finish(&tx, 1, "review", Some(ID)).unwrap();
        let resume = Request {
            expected: Some(ID),
            ..new
        };
        reserve(&tx, 2, 1, "codex", resume).unwrap();
        assert!(finish(&tx, 2, "review", Some(OTHER)).is_err());
        finish(&tx, 2, "review", Some(ID)).unwrap();
        assert!(reserve(&tx, 3, 2, "codex", resume).is_err());
        assert!(
            check(
                &tx,
                1,
                "codex",
                Request {
                    workspace: "/trusted/two",
                    ..resume
                }
            )
            .is_err()
        );
        reserve(&tx, 4, 2, "claude", resume).unwrap();
        tx.commit().unwrap();
    }
}
