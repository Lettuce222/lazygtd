//! Local question queue. Answering records a decision only; it never executes a tool.
use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};
use std::fmt;
use std::time::{SystemTime, UNIX_EPOCH};

pub const SCHEMA: &str = "
CREATE TABLE local_question (
    id INTEGER PRIMARY KEY,
    item_id INTEGER REFERENCES item(id),
    kind TEXT NOT NULL CHECK(kind IN ('desk','approval','permission','worker')),
    title TEXT NOT NULL,
    body TEXT NOT NULL,
    choices TEXT NOT NULL CHECK(json_valid(choices)),
    hidden INTEGER NOT NULL CHECK(hidden IN (0,1)),
    created_at INTEGER NOT NULL CHECK(created_at >= 0),
    deadline INTEGER,
    answer_option INTEGER CHECK(answer_option BETWEEN 1 AND 9),
    answer_text TEXT,
    answered_at INTEGER,
    CHECK((kind='permission' AND deadline IS NOT NULL AND deadline > created_at)
          OR (kind <> 'permission' AND deadline IS NULL)),
    CHECK((answered_at IS NULL AND answer_option IS NULL AND answer_text IS NULL)
       OR (answered_at IS NOT NULL AND (answer_option IS NOT NULL OR answer_text IS NOT NULL))),
    CHECK(answered_at IS NULL OR (answered_at >= created_at AND
                                  (deadline IS NULL OR answered_at < deadline)))
) STRICT;
CREATE INDEX local_question_pending ON local_question(hidden,deadline,id)
    WHERE answered_at IS NULL;
";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Kind {
    Desk,
    Approval,
    Permission,
    Worker,
}

impl Kind {
    pub fn label(self) -> &'static str {
        match self {
            Self::Desk => "エージェントの問い",
            Self::Approval => "承認の問い",
            Self::Permission => "許可の確認",
            Self::Worker => "worker の問い",
        }
    }

    fn key(self) -> &'static str {
        match self {
            Self::Desk => "desk",
            Self::Approval => "approval",
            Self::Permission => "permission",
            Self::Worker => "worker",
        }
    }

    fn from_key(key: &str) -> rusqlite::Result<Self> {
        match key {
            "desk" => Ok(Self::Desk),
            "approval" => Ok(Self::Approval),
            "permission" => Ok(Self::Permission),
            "worker" => Ok(Self::Worker),
            _ => Err(rusqlite::Error::InvalidQuery),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Choice {
    pub id: u8,
    pub label: String,
    pub recommended: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Question {
    pub id: i64,
    pub item_id: Option<i64>,
    pub kind: Kind,
    pub title: String,
    pub body: String,
    pub choices: Vec<Choice>,
    pub created_at: i64,
    pub deadline: Option<i64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Answer<'a> {
    Choice(u8),
    Text(&'a str),
    Reason(&'a str),
}

#[derive(Debug)]
pub enum Error {
    Unavailable,
    Expired,
    Answered,
    Invalid(&'static str),
    Sql(rusqlite::Error),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Unavailable => f.write_str("問いは表示・回答できません"),
            Self::Expired => f.write_str("許可の回答期限を過ぎています"),
            Self::Answered => f.write_str("この問いにはすでに回答があります"),
            Self::Invalid(message) => f.write_str(message),
            Self::Sql(error) => error.fmt(f),
        }
    }
}

impl std::error::Error for Error {}

impl From<rusqlite::Error> for Error {
    fn from(error: rusqlite::Error) -> Self {
        Self::Sql(error)
    }
}

pub type Result<T> = std::result::Result<T, Error>;

pub fn now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| {
            i64::try_from(elapsed.as_secs()).unwrap_or(i64::MAX)
        })
}

fn choice(id: u8, label: &str, recommended: bool) -> Choice {
    Choice {
        id,
        label: label.into(),
        recommended,
    }
}

fn fixed_choices(kind: Kind) -> Option<Vec<Choice>> {
    match kind {
        Kind::Approval => Some(vec![
            choice(1, "承認（記録のみ）", false),
            choice(2, "見送る", false),
        ]),
        Kind::Permission => Some(vec![
            choice(1, "許可（記録のみ）", false),
            choice(2, "拒否", false),
            choice(3, "理由を書く", false),
            choice(4, "ターミナルで決める（引継ぎを記録）", false),
        ]),
        _ => None,
    }
}

struct Draft<'a> {
    item_id: Option<i64>,
    kind: Kind,
    title: &'a str,
    body: &'a str,
    choices: &'a [Choice],
    hidden: bool,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ImportQuestion {
    pub item_id: Option<i64>,
    pub kind: Kind,
    pub title: String,
    pub body: String,
    #[serde(default)]
    pub choices: Vec<Choice>,
    #[serde(default)]
    pub hidden: bool,
}

pub fn register(c: &Connection, new: &ImportQuestion, now: i64) -> Result<i64> {
    insert(
        c,
        Draft {
            item_id: new.item_id,
            kind: new.kind,
            title: &new.title,
            body: &new.body,
            choices: &new.choices,
            hidden: new.hidden,
        },
        now,
    )
}

fn safe_text(value: &str, max: usize, multiline: bool) -> bool {
    !value.trim().is_empty()
        && value.len() <= max
        && value
            .chars()
            .all(|c| !c.is_control() || (multiline && matches!(c, '\n' | '\t')))
}

fn insert(c: &Connection, draft: Draft<'_>, now: i64) -> Result<i64> {
    let Draft {
        item_id,
        kind,
        title,
        body,
        choices,
        hidden,
    } = draft;
    let choices = fixed_choices(kind).unwrap_or_else(|| choices.to_vec());
    if !safe_text(title, 512, false)
        || !safe_text(body, 16_384, true)
        || choices.len() > 8
        || choices.iter().filter(|choice| choice.recommended).count() > 1
        || choices.iter().enumerate().any(|(index, choice)| {
            usize::from(choice.id) != index + 1 || !safe_text(&choice.label, 256, false)
        })
    {
        return Err(Error::Invalid("問いの本文または選択肢が不正です"));
    }
    let deadline = if kind == Kind::Permission {
        Some(
            now.checked_add(180)
                .ok_or(Error::Invalid("回答期限が不正です"))?,
        )
    } else {
        None
    };
    let choices =
        serde_json::to_string(&choices).map_err(|_| Error::Invalid("選択肢を保存できません"))?;
    c.execute(
        "INSERT INTO local_question(item_id,kind,title,body,choices,hidden,created_at,deadline)
         VALUES (?1,?2,?3,?4,?5,?6,?7,?8)",
        params![
            item_id,
            kind.key(),
            title,
            body,
            choices,
            hidden,
            now,
            deadline
        ],
    )?;
    Ok(c.last_insert_rowid())
}

pub fn list(c: &Connection, now: i64) -> Result<Vec<Question>> {
    let mut statement = c.prepare(
        "SELECT id,item_id,kind,title,body,choices,created_at,deadline
         FROM local_question WHERE hidden=0 AND answered_at IS NULL
         AND (deadline IS NULL OR deadline>?1)
         ORDER BY deadline IS NULL, deadline, id",
    )?;
    let rows = statement.query_map([now], |row| {
        let kind = Kind::from_key(&row.get::<_, String>(2)?)?;
        let choices = if let Some(fixed) = fixed_choices(kind) {
            fixed
        } else {
            let json: String = row.get(5)?;
            serde_json::from_str(&json).map_err(|_| rusqlite::Error::InvalidQuery)?
        };
        Ok(Question {
            id: row.get(0)?,
            item_id: row.get(1)?,
            kind,
            title: row.get(3)?,
            body: row.get(4)?,
            choices,
            created_at: row.get(6)?,
            deadline: row.get(7)?,
        })
    })?;
    Ok(rows.collect::<rusqlite::Result<_>>()?)
}

pub fn answer(c: &Connection, id: i64, response: Answer<'_>, now: i64) -> Result<()> {
    let (kind, options, created, deadline, answered) = c
        .query_row(
            "SELECT kind,choices,created_at,deadline,answered_at FROM local_question WHERE id=?1 AND hidden=0",
            [id],
            |row| Ok((
                Kind::from_key(&row.get::<_, String>(0)?)?,
                row.get::<_, String>(1)?,
                row.get::<_, i64>(2)?,
                row.get::<_, Option<i64>>(3)?,
                row.get::<_, Option<i64>>(4)?,
            )),
        )
        .optional()?
        .ok_or(Error::Unavailable)?;
    if answered.is_some() {
        return Err(Error::Answered);
    }
    if deadline.is_some_and(|deadline| now >= deadline) {
        return Err(Error::Expired);
    }
    if now < created {
        return Err(Error::Invalid("回答時刻が問いより前です"));
    }
    let options = match fixed_choices(kind) {
        Some(options) => options,
        None => serde_json::from_str(&options).map_err(|_| Error::Invalid("選択肢を読めません"))?,
    };
    let (option, note) = match response {
        Answer::Choice(3) if kind == Kind::Permission => {
            return Err(Error::Invalid("理由を入力してください"));
        }
        Answer::Choice(code) if options.iter().any(|option| option.id == code) => {
            (Some(code), None)
        }
        Answer::Text(text)
            if matches!(kind, Kind::Desk | Kind::Worker) && safe_text(text, 16_000, true) =>
        {
            (None, Some(text.trim()))
        }
        Answer::Reason(text) if kind == Kind::Permission && safe_text(text, 16_000, true) => {
            (Some(3), Some(text.trim()))
        }
        _ => return Err(Error::Invalid("この問いにはその方法で回答できません")),
    };
    let updated = c.execute(
        "UPDATE local_question SET answer_option=?1,answer_text=?2,answered_at=?3
         WHERE id=?4 AND hidden=0 AND answered_at IS NULL
         AND (deadline IS NULL OR deadline>?3)",
        params![option, note, now, id],
    )?;
    if updated != 1 {
        return Err(Error::Answered);
    }
    Ok(())
}

pub fn seed_demo(c: &Connection, now: i64) -> Result<()> {
    let desk_choices = [
        choice(1, "概要を先に読む", true),
        choice(2, "詳細から読む", false),
    ];
    let worker_choices = [
        choice(1, "色の比較を続ける", true),
        choice(2, "いったん保留", false),
    ];
    for (kind, title, body, choices) in [
        (
            Kind::Desk,
            "どちらから確認しますか？",
            "## デモの問い\n短い概要と、詳しいメモがあります。",
            desk_choices.as_slice(),
        ),
        (
            Kind::Approval,
            "下書きを送る設定にしますか？",
            "## 模擬の承認\n送信先はサンプルです。答えても外へ送信しません。",
            &[][..],
        ),
        (
            Kind::Permission,
            "比較用のコマンドを許可しますか？",
            "## 模擬の許可確認\nここでは判断の記録だけを行い、コマンドは実行しません。",
            &[][..],
        ),
        (
            Kind::Worker,
            "次にどの比較を進めますか？",
            "## worker のデモ\n違いが見やすい例を追加するか判断してください。",
            worker_choices.as_slice(),
        ),
    ] {
        insert(
            c,
            Draft {
                item_id: None,
                kind,
                title,
                body,
                choices,
                hidden: false,
            },
            now,
        )?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cx() -> Connection {
        let cx = Connection::open_in_memory().unwrap();
        cx.execute_batch("PRAGMA foreign_keys=ON; PRAGMA trusted_schema=OFF; CREATE TABLE item(id INTEGER PRIMARY KEY);").unwrap();
        cx.execute_batch(SCHEMA).unwrap();
        cx
    }

    #[test]
    fn demo_models_four_distinct_questions_and_strict_deadline() {
        let c = cx();
        seed_demo(&c, 10).unwrap();
        let all = list(&c, 10).unwrap();
        assert_eq!(all.len(), 4);
        assert_eq!(all[0].kind, Kind::Permission);
        assert_eq!(all[0].deadline, Some(190));
        assert_eq!(
            all[0].choices.iter().map(|x| x.id).collect::<Vec<_>>(),
            [1, 2, 3, 4]
        );
        assert_eq!(all[1].kind, Kind::Desk);
        assert!(all[1].choices[0].recommended);
        assert!(matches!(
            answer(&c, all[0].id, Answer::Choice(1), 190),
            Err(Error::Expired)
        ));
        assert_eq!(list(&c, 190).unwrap().len(), 3);
        answer(&c, all[0].id, Answer::Choice(2), 189).unwrap();
        assert!(matches!(
            answer(&c, all[0].id, Answer::Choice(1), 189),
            Err(Error::Answered)
        ));
        let saved: i64 = c
            .query_row(
                "SELECT answer_option FROM local_question WHERE id=?1",
                [all[0].id],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(saved, 2);
    }

    #[test]
    fn plaintext_reply_cannot_authorize_or_answer_hidden_questions() {
        let c = cx();
        seed_demo(&c, 100).unwrap();
        let all = list(&c, 110).unwrap();
        let approval = all.iter().find(|q| q.kind == Kind::Approval).unwrap();
        let worker = all.iter().find(|q| q.kind == Kind::Worker).unwrap();
        assert!(matches!(
            answer(&c, approval.id, Answer::Text("許可"), 110),
            Err(Error::Invalid(_))
        ));
        answer(
            &c,
            worker.id,
            Answer::Text("この方針で。\n補足があります。"),
            110,
        )
        .unwrap();
        let hidden = insert(
            &c,
            Draft {
                item_id: None,
                kind: Kind::Approval,
                title: "非表示",
                body: "非表示",
                choices: &[],
                hidden: true,
            },
            100,
        )
        .unwrap();
        assert!(!list(&c, 110).unwrap().iter().any(|q| q.id == hidden));
        assert!(matches!(
            answer(&c, hidden, Answer::Choice(1), 110),
            Err(Error::Unavailable)
        ));
        assert!(list(&c, 110).unwrap().iter().any(|q| q.id == approval.id));
    }

    #[test]
    fn simultaneous_opposite_answers_have_exactly_one_winner() {
        use std::sync::{Arc, Barrier};
        use std::thread;
        use std::time::Duration;

        let path = std::env::temp_dir().join(format!(
            "lazygtd-question-race-{}.sqlite3",
            std::process::id()
        ));
        let cx = Connection::open(&path).unwrap();
        cx.execute_batch("CREATE TABLE item(id INTEGER PRIMARY KEY);")
            .unwrap();
        cx.execute_batch(SCHEMA).unwrap();
        seed_demo(&cx, 100).unwrap();
        let id = list(&cx, 110).unwrap()[0].id;
        let barrier = Arc::new(Barrier::new(2));
        let attempts = [1_u8, 2]
            .map(|code| {
                let path = path.clone();
                let barrier = Arc::clone(&barrier);
                thread::spawn(move || {
                    let connection = Connection::open(path).unwrap();
                    connection.busy_timeout(Duration::from_secs(3)).unwrap();
                    barrier.wait();
                    (code, answer(&connection, id, Answer::Choice(code), 110))
                })
            })
            .map(|handle| handle.join().unwrap());
        let winners: Vec<_> = attempts
            .iter()
            .filter(|(_, result)| result.is_ok())
            .collect();
        assert_eq!(winners.len(), 1);
        assert!(
            attempts
                .iter()
                .any(|(_, result)| matches!(result, Err(Error::Answered)))
        );
        let saved: u8 = cx
            .query_row(
                "SELECT answer_option FROM local_question WHERE id=?1",
                [id],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(saved, winners[0].0);
        drop(cx);
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn reason_is_not_allow_and_requires_explicit_text() {
        let c = cx();
        seed_demo(&c, 100).unwrap();
        let permission = &list(&c, 110).unwrap()[0];
        assert!(matches!(
            answer(&c, permission.id, Answer::Choice(3), 110),
            Err(Error::Invalid(_))
        ));
        assert!(matches!(
            answer(&c, permission.id, Answer::Reason("　"), 110),
            Err(Error::Invalid(_))
        ));
        answer(
            &c,
            permission.id,
            Answer::Reason("判断材料が足りません"),
            110,
        )
        .unwrap();
        let saved: (i64, String) = c
            .query_row(
                "SELECT answer_option,answer_text FROM local_question WHERE id=?1",
                [permission.id],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(saved, (3, "判断材料が足りません".into()));
    }
}
