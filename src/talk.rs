use rusqlite::{Connection, params};
use serde::Serialize;

pub const SCHEMA: &str = "
CREATE TABLE task_message(
 id INTEGER PRIMARY KEY,
 item_id INTEGER NOT NULL REFERENCES item(id),
 role TEXT NOT NULL CHECK(role IN ('user','agent')),
 body TEXT NOT NULL CHECK(length(body)>0),
 run_id INTEGER REFERENCES agent_run(id),
 read INTEGER NOT NULL CHECK(read IN (0,1)),
 written_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ','now'))
) STRICT;
CREATE INDEX message_for_item ON task_message(item_id,id);
CREATE INDEX message_status ON task_message(item_id,role,run_id,read);
";

#[derive(Debug, Serialize)]
pub struct Message {
    pub id: i64,
    pub role: String,
    pub body: String,
    pub run_id: Option<i64>,
    pub read: bool,
    pub written_at: String,
}

pub fn list(c: &Connection, id: i64) -> rusqlite::Result<Vec<Message>> {
    let mut stmt = c.prepare(
        "SELECT id,role,body,run_id,read,written_at FROM
      (SELECT * FROM task_message WHERE item_id=? ORDER BY id DESC LIMIT 40) ORDER BY id",
    )?;
    stmt.query_map([id], |r| {
        Ok(Message {
            id: r.get(0)?,
            role: r.get(1)?,
            body: r.get(2)?,
            run_id: r.get(3)?,
            read: r.get(4)?,
            written_at: r.get(5)?,
        })
    })?
    .collect()
}

pub fn user(c: &Connection, id: i64, text: &str) -> rusqlite::Result<()> {
    c.execute(
        "INSERT INTO task_message(item_id,role,body,read) VALUES(?,'user',?,1)",
        params![id, text],
    )?;
    Ok(())
}

pub fn agent(c: &Connection, id: i64, run: i64, text: &str) -> rusqlite::Result<()> {
    c.execute(
        "INSERT INTO task_message(item_id,role,body,run_id,read) VALUES(?,'agent',?,?,0)",
        params![id, text, run],
    )?;
    Ok(())
}

pub fn claim(c: &Connection, id: i64, run: i64) -> rusqlite::Result<()> {
    c.execute(
        "UPDATE task_message SET run_id=? WHERE item_id=? AND role='user' AND run_id IS NULL",
        params![run, id],
    )?;
    Ok(())
}

pub fn mark_read(c: &Connection, id: i64, through: i64) -> rusqlite::Result<()> {
    c.execute(
        "UPDATE task_message SET read=1 WHERE item_id=? AND id<=? AND role='agent' AND read=0",
        params![id, through],
    )?;
    Ok(())
}
