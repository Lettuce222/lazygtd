//! Local project and waiting-list rules. All mutations accept the caller's immediate
//! transaction so a promotion, task changes and audit records commit together.
use rusqlite::{Connection, OptionalExtension, Transaction, params};
use serde::{Deserialize, Serialize};

use crate::store::{Result, StoreError};

pub const SCHEMA: &str = "
CREATE TABLE project_step (
    project_id INTEGER NOT NULL REFERENCES item(id),
    step_id INTEGER NOT NULL UNIQUE REFERENCES item(id),
    position INTEGER NOT NULL CHECK(position >= 0),
    PRIMARY KEY(project_id,position),
    CHECK (project_id != step_id)
) STRICT;
CREATE TABLE planning_change (
    item_id INTEGER NOT NULL REFERENCES item(id),
    revision INTEGER NOT NULL,
    kind TEXT NOT NULL CHECK(kind IN ('step-attach','step-detach','step-reorder','plan-meta')),
    occurred_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ','now')),
    PRIMARY KEY(item_id,revision)
) STRICT;
";

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Meta {
    pub due: Option<String>,
    pub calendar_action: bool,
    pub since: Option<String>,
    pub follow_up: Option<String>,
    pub who: Option<String>,
    pub then: Option<String>,
    pub watch: Option<String>,
    pub outcome: Option<String>,
    pub short: Option<String>,
    pub links: Vec<String>,
    pub source: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Step {
    pub id: i64,
    pub revision: i64,
    pub title: String,
    pub state: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PlanField {
    Due,
    FollowUp,
    Who,
    Then,
    Watch,
    Outcome,
    Short,
    Links,
    Since,
    Source,
}

impl PlanField {
    pub fn parse(key: &str) -> Result<Self> {
        Ok(match key {
            "due" => Self::Due,
            "follow_up" => Self::FollowUp,
            "who" => Self::Who,
            "then" => Self::Then,
            "watch" => Self::Watch,
            "outcome" => Self::Outcome,
            "short" => Self::Short,
            "links" => Self::Links,
            "since" => Self::Since,
            "source" => Self::Source,
            _ => return Err(StoreError::Invalid("unknown planning field")),
        })
    }

    pub fn key(self) -> &'static str {
        match self {
            Self::Due => "due",
            Self::FollowUp => "follow_up",
            Self::Who => "who",
            Self::Then => "then",
            Self::Watch => "watch",
            Self::Outcome => "outcome",
            Self::Short => "short",
            Self::Links => "links",
            Self::Since => "since",
            Self::Source => "source",
        }
    }
}

pub fn today(c: &Connection) -> Result<String> {
    Ok(c.query_row("SELECT date('now','localtime')", [], |r| r.get(0))?)
}

pub fn review_reasons(meta: &Meta, box_id: &str, current_date: &str) -> Vec<&'static str> {
    let mut reasons = Vec::with_capacity(2);
    if let Some(due) = meta.due.as_deref()
        && due <= current_date
    {
        reasons.push("締切");
    }
    if box_id == "waiting"
        && let Some(follow_up) = meta.follow_up.as_deref()
        && follow_up <= current_date
    {
        reasons.push("連絡確認");
    }
    reasons
}

#[cfg(test)]
pub fn metadata(c: &Connection, id: i64) -> Result<Meta> {
    let json: String = c
        .query_row(
            "SELECT coalesce(json_extract(details,'$.planning'),'{}') FROM item WHERE id=?",
            [id],
            |r| r.get(0),
        )
        .optional()?
        .ok_or(StoreError::Missing(id))?;
    serde_json::from_str(&json).map_err(|_| StoreError::Invalid("invalid planning metadata"))
}

pub fn steps(c: &Connection, project: i64) -> Result<Vec<Step>> {
    let mut q = c.prepare(
        "SELECT i.id,i.revision,i.title,
             CASE WHEN i.archived=1 THEN 'archived'
                  WHEN i.done=1 THEN 'done'
                  WHEN i.box_id='someday' THEN 'planned'
                  WHEN i.box_id='review' THEN 'review'
                  ELSE i.box_id END
         FROM project_step s JOIN item i ON i.id=s.step_id
         WHERE s.project_id=? ORDER BY s.position",
    )?;
    Ok(q.query_map([project], |r| {
        Ok(Step {
            id: r.get(0)?,
            revision: r.get(1)?,
            title: r.get(2)?,
            state: r.get(3)?,
        })
    })?
    .collect::<rusqlite::Result<Vec<_>>>()?)
}

pub fn project_of(c: &Connection, id: i64) -> Result<Option<i64>> {
    Ok(c.query_row(
        "SELECT project_id FROM project_step WHERE step_id=?",
        [id],
        |r| r.get(0),
    )
    .optional()?)
}

#[cfg(test)]
pub fn is_planned(c: &Connection, id: i64) -> Result<bool> {
    Ok(c.query_row(
        "SELECT EXISTS(SELECT 1 FROM project_step s JOIN item i ON i.id=s.step_id
                       WHERE i.id=? AND i.done=0 AND i.box_id='someday')",
        [id],
        |r| r.get(0),
    )?)
}

/// Reject fields that only apply to a particular box.
pub fn check_box(field: PlanField, box_id: &str) -> Result<()> {
    if matches!(
        field,
        PlanField::FollowUp | PlanField::Who | PlanField::Then | PlanField::Watch
    ) && box_id != "waiting"
    {
        return Err(StoreError::Invalid(
            "waiting fields require the waiting box",
        ));
    }
    if matches!(field, PlanField::Outcome | PlanField::Short) && box_id != "projects" {
        return Err(StoreError::Invalid("that field requires a project"));
    }
    Ok(())
}

/// Validate a planning value and return the JSON stored for it (`null` clears).
pub fn normalize(field: PlanField, value: &str) -> Result<serde_json::Value> {
    let s = value.trim();
    Ok(match field {
        PlanField::Links => {
            let urls = s
                .lines()
                .map(str::trim)
                .filter(|line| !line.is_empty())
                .collect::<Vec<_>>();
            if urls.len() > 30 || urls.iter().any(|url| !valid_url(url)) {
                return Err(StoreError::Invalid(
                    "links must be HTTP(S) URLs, one per line (up to 30)",
                ));
            }
            serde_json::json!(urls)
        }
        _ if s.is_empty() => serde_json::Value::Null,
        PlanField::Due | PlanField::FollowUp | PlanField::Since => {
            if !valid_date(s) {
                return Err(StoreError::Invalid(
                    "use a real calendar date in YYYY-MM-DD format",
                ));
            }
            serde_json::json!(s)
        }
        PlanField::Watch => {
            let urls = s.split_whitespace().collect::<Vec<_>>();
            if urls.len() > 20 || urls.iter().any(|url| !valid_url(url)) {
                return Err(StoreError::Invalid(
                    "watch requires 1–20 HTTP(S) URLs separated by whitespace",
                ));
            }
            serde_json::json!(urls.join(" "))
        }
        _ => {
            let max = match field {
                PlanField::Short => 80,
                PlanField::Who => 200,
                _ => 2_000,
            };
            if s.len() > max || s.chars().any(char::is_control) {
                return Err(StoreError::Invalid(
                    "planning value is too long or contains control characters",
                ));
            }
            serde_json::json!(s)
        }
    })
}

pub fn change_meta(
    tx: &Transaction<'_>,
    id: i64,
    rev: i64,
    field: PlanField,
    value: &str,
) -> Result<i64> {
    let item = current(tx, id, rev, false)?;
    check_box(field, &item.bx)?;
    let value = normalize(field, value)?;
    let path = format!("$.planning.{}", field.key());
    let normalized = value.to_string();
    let previous: String = tx.query_row(
        "SELECT coalesce(details -> ?,'null') FROM item WHERE id=?",
        params![path, id],
        |r| r.get(0),
    )?;
    if previous == normalized
        || (field == PlanField::Links && previous == "null" && normalized == "[]")
    {
        return Ok(rev);
    }

    touch(tx, id, "plan-meta")?;
    tx.execute(
        "UPDATE item SET details=json_set(details,?,json(?)) WHERE id=?",
        params![path, normalized, id],
    )?;
    revision(tx, id)
}

/// Convert a task to the local next action "put it on the calendar".
/// No dates are invented and no external calendar is opened or modified.
pub fn mark_calendar(tx: &Transaction<'_>, id: i64, rev: i64, actor: &str) -> Result<()> {
    let item = current(tx, id, rev, false)?;
    if item.bx == "projects" {
        return Err(StoreError::Invalid(
            "create a next action for a project rather than converting the project",
        ));
    }

    let flagged: bool = tx.query_row(
        "SELECT coalesce(json_extract(details,'$.planning.calendar_action'),0)=1 FROM item WHERE id=?",
        [id],
        |r| r.get(0),
    )?;
    if flagged && item.bx == "next" {
        return Ok(());
    }
    before_item_change(tx, id, Some("next"))?;
    if item.bx == "next" {
        touch(tx, id, "plan-meta")?;
    } else {
        set_box(tx, id, "next", actor)?;
    }
    tx.execute(
        "UPDATE item SET details=json_set(details,'$.planning.calendar_action',json('true')) WHERE id=?",
        [id],
    )?;
    Ok(())
}

pub fn attach(
    tx: &Transaction<'_>,
    project: i64,
    project_rev: i64,
    step: i64,
    step_rev: i64,
) -> Result<()> {
    ensure_project(tx, project, project_rev)?;
    let s = current(tx, step, step_rev, true)?;
    if project == step || s.bx == "projects" {
        return Err(StoreError::Invalid(
            "projects cannot be nested or attached to themselves",
        ));
    }
    if project_of(tx, step)?.is_some() {
        return Err(StoreError::Invalid("step already belongs to a project"));
    }
    let index: i64 = tx.query_row(
        "SELECT coalesce(max(position)+1,0) FROM project_step WHERE project_id=?",
        [project],
        |r| r.get(0),
    )?;
    if index >= 1_000_000 {
        return Err(StoreError::Invalid("too many project steps"));
    }
    tx.execute(
        "INSERT INTO project_step(project_id,step_id,position) VALUES(?,?,?)",
        params![project, step, index],
    )?;
    touch(tx, project, "step-attach")?;
    touch(tx, step, "step-attach")?;
    if !s.done && !matches!(s.bx.as_str(), "next" | "waiting" | "someday" | "review") {
        set_box(tx, step, "someday", SYSTEM)?;
    }
    promote(tx, project)
}

pub fn detach(
    tx: &Transaction<'_>,
    project: i64,
    project_rev: i64,
    step: i64,
    step_rev: i64,
) -> Result<()> {
    ensure_project(tx, project, project_rev)?;
    current(tx, step, step_rev, true)?;
    if tx.execute(
        "DELETE FROM project_step WHERE project_id=? AND step_id=?",
        params![project, step],
    )? == 0
    {
        return Err(StoreError::Invalid("step does not belong to this project"));
    }
    touch(tx, project, "step-detach")?;
    touch(tx, step, "step-detach")?;
    promote(tx, project)
}

pub fn reorder(tx: &Transaction<'_>, project: i64, project_rev: i64, order: &[i64]) -> Result<()> {
    ensure_project(tx, project, project_rev)?;
    let current = steps(tx, project)?
        .into_iter()
        .map(|step| step.id)
        .collect::<Vec<_>>();
    if current == order {
        return Ok(());
    }
    let mut actual = order.to_vec();
    let mut expected = current;
    actual.sort_unstable();
    expected.sort_unstable();
    if actual != expected {
        return Err(StoreError::Invalid(
            "reorder requires every step exactly once",
        ));
    }
    // Temporarily park positions outside the destination range so UNIQUE is never violated.
    tx.execute(
        "UPDATE project_step SET position=position+1000000 WHERE project_id=?",
        [project],
    )?;
    for (index, step) in order.iter().enumerate() {
        tx.execute(
            "UPDATE project_step SET position=? WHERE project_id=? AND step_id=?",
            params![index as i64, project, step],
        )?;
    }
    touch(tx, project, "step-reorder")?;
    promote(tx, project)
}

/// Before a user move/done; explicitly distinguish planned steps from plain someday.
pub fn before_item_change(tx: &Transaction<'_>, id: i64, dest: Option<&str>) -> Result<()> {
    if let Some(dest) = dest {
        if project_of(tx, id)?.is_some() && !matches!(dest, "next" | "waiting" | "someday" | "review") {
            return Err(StoreError::Invalid("project steps can move to next, waiting, planned or review; detach first for other lists"));
        }
        if dest != "projects"
            && tx.query_row("SELECT EXISTS(SELECT 1 FROM project_step WHERE project_id=?)", [id], |r| r.get::<_,bool>(0))? {
            return Err(StoreError::Invalid("detach project steps before moving the project"));
        }
    } else if tx.query_row(
        "SELECT EXISTS(SELECT 1 FROM project_step s JOIN item i ON i.id=s.step_id WHERE s.project_id=? AND i.done=0 AND i.archived=0)",
        [id], |r| r.get::<_,bool>(0),
    )? {
        return Err(StoreError::Invalid("complete or detach the project's remaining steps first"));
    }
    Ok(())
}

/// After a changed item and the original move/done event, inside the same transaction.
pub fn on_item_changed(tx: &Transaction<'_>, id: i64) -> Result<()> {
    update_since(tx, id)?;
    if let Some(project) = project_of(tx, id)? {
        promote(tx, project)?;
    }
    Ok(())
}

/// Does not touch tasks held for human review; these must be completed or moved first.
fn promote(tx: &Transaction<'_>, project: i64) -> Result<()> {
    let next: Option<i64> = tx
        .query_row(
            "SELECT s.step_id
         FROM project_step s JOIN item i ON i.id=s.step_id
         WHERE s.project_id=?1 AND i.done=0 AND i.archived=0 AND i.box_id='someday'
           AND NOT EXISTS (
              SELECT 1 FROM project_step a JOIN item b ON b.id=a.step_id
              WHERE a.project_id=?1 AND b.done=0 AND b.archived=0 AND (b.box_id IN ('next','review')
                  OR EXISTS(SELECT 1 FROM agent_run r WHERE r.item_id=b.id AND r.state='running'))
           )
         ORDER BY s.position LIMIT 1",
            [project],
            |r| r.get(0),
        )
        .optional()?;
    if let Some(next) = next {
        set_box(tx, next, "next", SYSTEM)?;
    }
    Ok(())
}

fn ensure_project(tx: &Transaction<'_>, id: i64, rev: i64) -> Result<()> {
    let p = current(tx, id, rev, false)?;
    if p.bx != "projects" {
        return Err(StoreError::Invalid(
            "choose an open item in the projects box",
        ));
    }
    Ok(())
}

struct Current {
    bx: String,
    done: bool,
}

fn current(tx: &Transaction<'_>, id: i64, expect: i64, allow_done: bool) -> Result<Current> {
    let (rev, bx, done, archived, active): (i64, String, bool, bool, bool) = tx
        .query_row(
            "SELECT revision,box_id,done,archived,
                EXISTS(SELECT 1 FROM agent_run WHERE item_id=?1 AND state='running')
         FROM item WHERE id=?1",
            [id],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)),
        )
        .optional()?
        .ok_or(StoreError::Missing(id))?;
    if rev != expect {
        return Err(StoreError::Stale {
            id,
            expected: expect,
            current: rev,
        });
    }
    if archived {
        return Err(StoreError::Invalid(
            "archived items cannot be changed; reopen first",
        ));
    }
    if done && !allow_done {
        return Err(StoreError::Invalid("completed items cannot be changed"));
    }
    if active {
        return Err(StoreError::Invalid("an agent is running on this item"));
    }
    Ok(Current { bx, done })
}

fn revision(c: &Connection, id: i64) -> Result<i64> {
    Ok(c.query_row("SELECT revision FROM item WHERE id=?", [id], |r| r.get(0))?)
}

fn touch(tx: &Transaction<'_>, id: i64, kind: &str) -> Result<()> {
    tx.execute("UPDATE item SET revision=revision+1 WHERE id=?", [id])?;
    tx.execute(
        "INSERT INTO planning_change (item_id,revision,kind)
         SELECT id,revision,? FROM item WHERE id=?",
        params![kind, id],
    )?;
    Ok(())
}

const SYSTEM: &str = "system:planning";

fn set_box(tx: &Transaction<'_>, id: i64, name: &str, actor: &str) -> Result<()> {
    tx.execute(
        "UPDATE item SET revision=revision+1,box_id=? WHERE id=?",
        params![name, id],
    )?;
    tx.execute(
        "INSERT INTO event(item_id,revision,actor,kind,box_id,done)
         SELECT id,revision,?,'move',box_id,done FROM item WHERE id=?",
        params![actor, id],
    )?;
    update_since(tx, id)
}

fn update_since(tx: &Transaction<'_>, id: i64) -> Result<()> {
    tx.execute(
        "UPDATE item SET details=json_set(details,'$.planning.since',date('now','localtime')) WHERE id=?",
        [id],
    )?;
    Ok(())
}

fn valid_date(s: &str) -> bool {
    let b = s.as_bytes();
    if b.len() != 10
        || b[4] != b'-'
        || b[7] != b'-'
        || b.iter()
            .enumerate()
            .any(|(i, c)| i != 4 && i != 7 && !c.is_ascii_digit())
    {
        return false;
    }
    let year = s[..4].parse::<u32>().unwrap_or(0);
    let month = s[5..7].parse::<u32>().unwrap_or(0);
    let day = s[8..].parse::<u32>().unwrap_or(0);
    let leap = year.is_multiple_of(4) && (!year.is_multiple_of(100) || year.is_multiple_of(400));
    let max = match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 if leap => 29,
        2 => 28,
        _ => 0,
    };
    year > 0 && day > 0 && day <= max
}

fn valid_url(s: &str) -> bool {
    if s.len() > 2_000 || s.chars().any(|c| c.is_control() || c.is_whitespace()) {
        return false;
    }
    let authority = s
        .strip_prefix("https://")
        .or_else(|| s.strip_prefix("http://"))
        .and_then(|rest| rest.split(['/', '?', '#']).next());
    authority.is_some_and(|host| !host.is_empty() && !host.contains(['@', '\\']))
}

#[cfg(test)]
mod tests {
    use super::*;
    use rusqlite::TransactionBehavior;

    fn db() -> Connection {
        let c = Connection::open_in_memory().unwrap();
        c.execute_batch("
            PRAGMA foreign_keys=ON;
            CREATE TABLE item(id INTEGER PRIMARY KEY,revision INTEGER NOT NULL, title TEXT NOT NULL,
               box_id TEXT NOT NULL, done INTEGER NOT NULL DEFAULT 0, archived INTEGER NOT NULL DEFAULT 0,
               details TEXT NOT NULL DEFAULT '{}');
            CREATE TABLE event(item_id INTEGER, revision INTEGER, actor TEXT, kind TEXT, box_id TEXT,
               done INTEGER, UNIQUE(item_id,revision));
            CREATE TABLE agent_run(item_id INTEGER, state TEXT);
            INSERT INTO item(id,revision,title,box_id) VALUES
               (1,1,'Project','projects'), (2,1,'First','inbox'), (3,1,'Second','inbox'),
               (4,1,'Third','inbox'), (5,1,'Waiting outside','waiting'), (6,1,'Other project','projects'),
               (7,1,'Plain someday','someday');
        ").unwrap();
        c.execute_batch(SCHEMA).unwrap();
        c
    }

    fn transaction() -> TransactionBehavior {
        TransactionBehavior::Immediate
    }

    fn states(c: &Connection) -> Vec<(i64, String)> {
        steps(c, 1)
            .unwrap()
            .into_iter()
            .map(|s| (s.id, s.state))
            .collect()
    }

    #[test]
    fn planned_steps_promote_in_order_after_completed_human_review() {
        let mut c = db();
        let t = c.transaction_with_behavior(transaction()).unwrap();
        attach(&t, 1, 1, 2, 1).unwrap();
        let r = revision(&t, 1).unwrap();
        attach(&t, 1, r, 3, 1).unwrap();
        let r = revision(&t, 1).unwrap();
        attach(&t, 1, r, 4, 1).unwrap();
        assert_eq!(
            states(&t),
            [
                (2, "next".into()),
                (3, "planned".into()),
                (4, "planned".into())
            ]
        );
        assert!(is_planned(&t, 3).unwrap());
        assert!(!is_planned(&t, 7).unwrap());
        assert_eq!(project_of(&t, 3).unwrap(), Some(1));
        let r = revision(&t, 1).unwrap();
        assert!(matches!(
            reorder(&t, 1, r - 1, &[4, 3, 2]),
            Err(StoreError::Stale { .. })
        ));
        assert!(reorder(&t, 1, r, &[4, 4, 2]).is_err());
        reorder(&t, 1, r, &[2, 4, 3]).unwrap();
        assert!(before_item_change(&t, 1, None).is_err());
        assert!(before_item_change(&t, 1, Some("next")).is_err());
        assert!(before_item_change(&t, 2, Some("reference")).is_err());
        // Model a successful agent handback, then the user's completing that review.
        t.execute(
            "UPDATE item SET box_id='review',revision=revision+1 WHERE id=2",
            [],
        )
        .unwrap();
        on_item_changed(&t, 2).unwrap();
        assert_eq!(
            states(&t),
            [
                (2, "review".into()),
                (4, "planned".into()),
                (3, "planned".into())
            ]
        );
        t.execute("UPDATE item SET done=1,revision=revision+1 WHERE id=2", [])
            .unwrap();
        on_item_changed(&t, 2).unwrap();
        assert_eq!(
            states(&t),
            [
                (2, "done".into()),
                (4, "next".into()),
                (3, "planned".into())
            ]
        );
        let p_rev = revision(&t, 1).unwrap();
        let step_rev = revision(&t, 4).unwrap();
        detach(&t, 1, p_rev, 4, step_rev).unwrap();
        assert_eq!(states(&t), [(2, "done".into()), (3, "next".into())]);
        assert_eq!(project_of(&t, 4).unwrap(), None);
        assert!(metadata(&t, 3).unwrap().since.is_some());
        t.commit().unwrap();
        assert_eq!(states(&c), [(2, "done".into()), (3, "next".into())]);
    }

    #[test]
    fn metadata_validates_calendar_and_retains_body_unknown_keys_and_wait_state() {
        let mut c = db();
        let t = c.transaction_with_behavior(transaction()).unwrap();
        t.execute(
            "UPDATE item SET details=json_set(details,'$.body','memo','$.planning.custom','retained') WHERE id=5",
            [],
        ).unwrap();
        assert!(change_meta(&t, 5, 1, PlanField::Due, "2026-02-29").is_err());
        assert!(change_meta(&t, 5, 1, PlanField::Due, "0000-01-01").is_err());
        assert!(change_meta(&t, 5, 1, PlanField::Due, "２０２６-11-25").is_err());
        assert!(change_meta(&t, 2, 1, PlanField::Who, "someone").is_err());
        assert!(change_meta(&t, 5, 1, PlanField::Watch, "https://a:b@host.invalid/").is_err());
        assert_eq!(revision(&t, 5).unwrap(), 1);
        let r = change_meta(&t, 5, 1, PlanField::parse("due").unwrap(), "2028-02-29").unwrap();
        assert_eq!(r, 2);
        let r = change_meta(&t, 5, r, PlanField::FollowUp, "2028-02-28").unwrap();
        let r = change_meta(
            &t,
            5,
            r,
            PlanField::Links,
            "https://example.invalid/1\nhttps://example.invalid/2",
        )
        .unwrap();
        let m = metadata(&t, 5).unwrap();
        assert_eq!(m.links.len(), 2);
        assert_eq!(
            review_reasons(&m, "waiting", "2028-02-29"),
            ["締切", "連絡確認"]
        );
        assert_eq!(review_reasons(&m, "next", "2028-02-29"), ["締切"]);
        assert!(review_reasons(&m, "waiting", "2028-02-27").is_empty());
        assert!(valid_date(&today(&t).unwrap()));
        let custom: (String,String) = t.query_row(
            "SELECT json_extract(details,'$.body'),json_extract(details,'$.planning.custom') FROM item WHERE id=5",[],
            |row| Ok((row.get(0)?,row.get(1)?)),
        ).unwrap();
        assert_eq!(custom, ("memo".into(), "retained".into()));
        assert_eq!(
            change_meta(&t, 5, r, PlanField::Due, "2028-02-29").unwrap(),
            r
        );
        assert_eq!(change_meta(&t, 5, r, PlanField::Due, "").unwrap(), r + 1);
        assert!(metadata(&t, 5).unwrap().due.is_none());
        t.commit().unwrap();
    }

    #[test]
    fn calendar_handoff_is_local_idempotent_and_retains_project_membership() {
        let mut c = db();
        let t = c.transaction_with_behavior(transaction()).unwrap();
        assert!(mark_calendar(&t, 1, 1, "user").is_err());
        mark_calendar(&t, 5, 1, "user").unwrap();
        assert_eq!(metadata(&t, 5).unwrap().due, None);
        assert!(metadata(&t, 5).unwrap().calendar_action);
        assert!(metadata(&t, 5).unwrap().since.is_some());
        assert_eq!(revision(&t, 5).unwrap(), 2);
        assert!(matches!(
            mark_calendar(&t, 5, 1, "user"),
            Err(StoreError::Stale { .. })
        ));
        mark_calendar(&t, 5, 2, "user").unwrap();
        assert_eq!(revision(&t, 5).unwrap(), 2);
        assert_eq!(current(&t, 5, 2, false).unwrap().bx, "next");
        let count: i64 = t
            .query_row("SELECT count(*) FROM event WHERE item_id=5", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(count, 1);

        attach(&t, 1, 1, 2, 1).unwrap();
        let rev = revision(&t, 2).unwrap();
        mark_calendar(&t, 2, rev, "user").unwrap();
        assert_eq!(project_of(&t, 2).unwrap(), Some(1));
        assert_eq!(states(&t), [(2, "next".into())]);
        assert!(metadata(&t, 2).unwrap().calendar_action);
        assert_eq!(revision(&t, 2).unwrap(), rev + 1);
        t.commit().unwrap();
    }

    #[test]
    fn failed_promotion_is_rolled_back_with_the_original_change() {
        let mut c = db();
        {
            let t = c.transaction_with_behavior(transaction()).unwrap();
            attach(&t, 1, 1, 2, 1).unwrap();
            attach(&t, 1, revision(&t, 1).unwrap(), 3, 1).unwrap();
            t.commit().unwrap();
        }
        c.execute_batch(
            "CREATE TRIGGER refuse_promotion BEFORE INSERT ON event
               WHEN new.item_id=3 BEGIN SELECT RAISE(ABORT,'test'); END;",
        )
        .unwrap();
        let t = c.transaction_with_behavior(transaction()).unwrap();
        t.execute("UPDATE item SET done=1 WHERE id=2", []).unwrap();
        assert!(on_item_changed(&t, 2).is_err());
        t.rollback().unwrap();
        assert_eq!(states(&c), [(2, "next".into()), (3, "planned".into())]);
    }
}
