use crate::{planning, questions, talk};
use std::{error::Error, fmt, path::Path, time::Duration};

use rusqlite::{Connection, OptionalExtension, TransactionBehavior, params};
use serde::Serialize;

/// Columns read by [`row`]; keep the order in sync with it.
macro_rules! select_item {
    () => {
        "SELECT id,revision,title,box_id,done,assignee,instructions,coalesce(json_extract(details,'$.body'),''), EXISTS(SELECT 1 FROM task_message m WHERE m.item_id=item.id AND m.role='user' AND m.run_id IS NULL), EXISTS(SELECT 1 FROM task_message m WHERE m.item_id=item.id AND m.role='agent' AND m.read=0), coalesce(json_extract(details,'$.planning'),'{}'), (SELECT project_id FROM project_step s WHERE s.step_id=item.id), coalesce(json_extract(details,'$.agent_review'),0), EXISTS(SELECT 1 FROM agent_run r WHERE r.item_id=item.id AND r.state='running'), archived FROM item"
    };
}

pub const BOX_IDS: [&str; 8] = [
    "review",
    "desk",
    "inbox",
    "next",
    "projects",
    "waiting",
    "someday",
    "reference",
];

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Item {
    pub id: i64,
    pub revision: i64,
    pub title: String,
    pub box_id: String,
    pub done: bool,
    pub archived: bool,
    pub assignee: String,
    pub instructions: String,
    pub body: String,
    pub pending_message: bool,
    pub unread_reply: bool,
    pub planning: planning::Meta,
    pub project_id: Option<i64>,
    pub agent_review: bool,
    pub agent_active: bool,
}

#[derive(Debug)]
pub enum StoreError {
    Missing(i64),
    Stale {
        id: i64,
        expected: i64,
        current: i64,
    },
    Invalid(&'static str),
    Sql(rusqlite::Error),
    Io(std::io::Error),
}

impl fmt::Display for StoreError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Missing(id) => write!(f, "item {id} does not exist"),
            Self::Stale {
                id,
                expected,
                current,
            } => write!(
                f,
                "item {id} changed (expected revision {expected}, current {current}); reload before retrying"
            ),
            Self::Invalid(s) => f.write_str(s),
            Self::Sql(e) => e.fmt(f),
            Self::Io(e) => e.fmt(f),
        }
    }
}
impl Error for StoreError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Sql(e) => Some(e),
            Self::Io(e) => Some(e),
            _ => None,
        }
    }
}
impl From<rusqlite::Error> for StoreError {
    fn from(e: rusqlite::Error) -> Self {
        Self::Sql(e)
    }
}

pub type Result<T> = std::result::Result<T, StoreError>;

pub trait Store {
    /// Items that are not archived, done or not.
    fn list(&self) -> Result<Vec<Item>>;
    /// Every item, archived ones included.
    fn list_all(&self) -> Result<Vec<Item>>;
    fn show(&self, id: i64) -> Result<Item>;
    fn add_with_body(&mut self, title: &str, body: &str, actor: &str) -> Result<Item> {
        self.create(&NewItem {
            title,
            body,
            actor,
            ..NewItem::default()
        })
    }
    fn create(&mut self, new: &NewItem<'_>) -> Result<Item>;
    fn edit_body(&mut self, id: i64, rev: i64, body: &str, actor: &str) -> Result<Item>;
    fn retitle(&mut self, id: i64, rev: i64, title: &str, actor: &str) -> Result<Item>;
    fn move_to(&mut self, id: i64, rev: i64, dest: &str, actor: &str) -> Result<Item>;
    fn done(&mut self, id: i64, rev: i64, actor: &str) -> Result<Item>;
    /// Close without completing. Events and links are kept.
    fn archive(&mut self, id: i64, rev: i64, actor: &str) -> Result<Item>;
    /// Clear done and archived; the item stays in the box it was closed in.
    fn reopen(&mut self, id: i64, rev: i64, actor: &str) -> Result<Item>;
    fn assign(&mut self, id: i64, rev: i64, who: &str, actor: &str) -> Result<Item>;
    fn instruct(&mut self, id: i64, rev: i64, text: &str, actor: &str) -> Result<Item>;
    fn conversation(&self, id: i64) -> Result<Vec<talk::Message>>;
    fn send_message(&mut self, id: i64, rev: i64, text: &str, actor: &str) -> Result<Item>;
    /// An agent's unread reply outside any run.
    fn reply(&mut self, id: i64, rev: i64, text: &str, actor: &str) -> Result<Item>;
    fn mark_read(&mut self, id: i64, through: i64) -> Result<()>;
    fn agent_prompt(&self, id: i64) -> Result<String>;
    fn run_input(&self, run: i64) -> Result<String>;
    fn calendar_action(&mut self, id: i64, rev: i64, actor: &str) -> Result<Item>;
    fn watch_resolve(&mut self, id: i64, rev: i64, closed: &[&str], actor: &str) -> Result<Item>;
    fn today(&self) -> Result<String>;
    fn steps(&self, id: i64) -> Result<Vec<planning::Step>>;
    fn change_meta(
        &mut self,
        id: i64,
        rev: i64,
        field: planning::PlanField,
        value: &str,
        actor: &str,
    ) -> Result<Item>;
    fn add_step(
        &mut self,
        project: i64,
        rev: i64,
        title: &str,
        body: &str,
        actor: &str,
    ) -> Result<Item>;
    fn attach(
        &mut self,
        project: i64,
        rev: i64,
        step: i64,
        step_rev: i64,
        actor: &str,
    ) -> Result<Item>;
    fn detach(
        &mut self,
        project: i64,
        rev: i64,
        step: i64,
        step_rev: i64,
        actor: &str,
    ) -> Result<Item>;
    fn reorder(&mut self, project: i64, rev: i64, order: &[i64], actor: &str) -> Result<Item>;
    fn questions(&self, now: i64) -> questions::Result<Vec<questions::Question>>;
    fn answer(&mut self, id: i64, answer: questions::Answer<'_>, now: i64)
    -> questions::Result<()>;
    fn latest_run(&self, id: i64) -> Result<Option<AgentRun>>;
    fn start_run(&mut self, id: i64, rev: i64, provider: &str) -> Result<i64> {
        self.start_run_scoped(id, rev, provider, None)
    }
    fn start_run_scoped(
        &mut self,
        id: i64,
        rev: i64,
        provider: &str,
        scope: Option<crate::sessions::Request<'_>>,
    ) -> Result<i64>;
    fn check_session(
        &self,
        id: i64,
        provider: &str,
        scope: crate::sessions::Request<'_>,
    ) -> Result<()>;
    fn progress(&mut self, run: i64, event: &str) -> Result<()>;
    fn finish_run(&mut self, run: i64, state: &str, result: &str) -> Result<()> {
        self.finish_run_scoped(run, state, result, None)
    }
    fn finish_run_scoped(
        &mut self,
        run: i64,
        state: &str,
        result: &str,
        native_session: Option<&str>,
    ) -> Result<()>;
}

pub struct Sqlite(Connection);

/// A new item with its box, assignee and planning fields, created in one transaction.
#[derive(Default)]
pub struct NewItem<'a> {
    pub title: &'a str,
    pub body: &'a str,
    pub box_id: Option<&'a str>,
    pub assignee: Option<&'a str>,
    pub meta: Vec<(planning::PlanField, String)>,
    pub actor: &'a str,
}

enum Change<'a> {
    Move(&'a str),
    Done,
    Archive,
}

#[derive(Debug, Serialize)]
pub struct AgentRun {
    pub id: i64,
    pub item_revision: i64,
    pub provider: String,
    pub state: String,
    pub activity: String,
    pub result: String,
    pub started_at: String,
    pub finished_at: Option<String>,
    pub session_id: Option<String>,
}

const V2: &str = "
ALTER TABLE item ADD COLUMN assignee TEXT NOT NULL DEFAULT 'me' CHECK(assignee IN ('me','agent'));
ALTER TABLE item ADD COLUMN instructions TEXT NOT NULL DEFAULT '';
CREATE TABLE task_change (
    item_id INTEGER NOT NULL REFERENCES item(id), revision INTEGER NOT NULL,
    kind TEXT NOT NULL CHECK(kind IN ('assign','instruct','handback')),
    PRIMARY KEY(item_id,revision)
) STRICT;
CREATE TABLE agent_run (
    id INTEGER PRIMARY KEY, item_id INTEGER NOT NULL REFERENCES item(id),
    item_revision INTEGER NOT NULL, provider TEXT NOT NULL CHECK(provider IN ('codex','claude','demo')),
    input TEXT NOT NULL, state TEXT NOT NULL CHECK(state IN ('running','review','failed')),
    activity TEXT NOT NULL DEFAULT 'starting', result TEXT NOT NULL DEFAULT '',
    started_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ','now')),
    finished_at TEXT
) STRICT;
CREATE UNIQUE INDEX agent_one_active ON agent_run(item_id) WHERE state='running';
CREATE INDEX agent_item_history ON agent_run(item_id,id);
";

const V3: &str = "
ALTER TABLE item ADD COLUMN details TEXT NOT NULL DEFAULT '{}'
    CHECK (json_valid(details) AND json_type(details)='object'
           AND coalesce(json_type(details,'$.body'),'text')='text');
ALTER TABLE task_change RENAME TO task_change_v2;
CREATE TABLE task_change (
    item_id INTEGER NOT NULL REFERENCES item(id), revision INTEGER NOT NULL,
    kind TEXT NOT NULL CHECK(kind IN ('assign','instruct','handback','edit-body')),
    PRIMARY KEY(item_id,revision)
) STRICT;
INSERT INTO task_change SELECT item_id,revision,kind FROM task_change_v2;
DROP TABLE task_change_v2;
";
/// v7: archived items and an event log whose kinds cover every item write.
const V7: &str = "
ALTER TABLE item ADD COLUMN archived INTEGER NOT NULL DEFAULT 0 CHECK(archived IN (0,1));
ALTER TABLE event RENAME TO event_v6;
CREATE TABLE event (
    seq INTEGER PRIMARY KEY, item_id INTEGER NOT NULL REFERENCES item(id),
    revision INTEGER NOT NULL, actor TEXT NOT NULL,
    kind TEXT NOT NULL CHECK(kind IN ('add','move','done','archive','reopen','retitle',
        'edit-body','assign','instruct','message','reply','plan-meta',
        'step-attach','step-detach','step-reorder')),
    box_id TEXT NOT NULL, done INTEGER NOT NULL,
    occurred_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ','now')),
    UNIQUE(item_id, revision)
) STRICT;
INSERT INTO event SELECT * FROM event_v6;
DROP TABLE event_v6;
";
const APP_ID: i64 = 0x4C475444;
const SCHEMA: &str = "
CREATE TABLE item (
    id INTEGER PRIMARY KEY, revision INTEGER NOT NULL CHECK (revision >= 1),
    title TEXT NOT NULL, box_id TEXT NOT NULL
        CHECK(box_id IN ('review','desk','inbox','next','projects','waiting','someday','reference')),
    done INTEGER NOT NULL CHECK(done IN (0,1))
) STRICT;
CREATE TABLE event (
    seq INTEGER PRIMARY KEY, item_id INTEGER NOT NULL REFERENCES item(id),
    revision INTEGER NOT NULL, actor TEXT NOT NULL,
    kind TEXT NOT NULL CHECK(kind IN ('add','move','done')),
    box_id TEXT NOT NULL, done INTEGER NOT NULL,
    occurred_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ','now')),
    UNIQUE(item_id, revision)
) STRICT;";

impl Sqlite {
    pub fn open(path: &Path) -> Result<Self> {
        // macOS exposes its system temp directory through /var -> /private/var.
        // Normalize only ancestors; never resolve the database leaf itself.
        #[cfg(unix)]
        let normalized = {
            let parent = path
                .parent()
                .filter(|x| !x.as_os_str().is_empty())
                .unwrap_or_else(|| Path::new("."));
            let leaf = path
                .file_name()
                .ok_or(StoreError::Invalid("database file name is missing"))?;
            parent.canonicalize().map_err(StoreError::Io)?.join(leaf)
        };
        #[cfg(unix)]
        let path = normalized.as_path();
        crate::db_file::prepare(path).map_err(StoreError::Io)?;
        let flags = rusqlite::OpenFlags::default() & !rusqlite::OpenFlags::SQLITE_OPEN_CREATE;
        Self::init(
            Connection::open_with_flags(path, flags | rusqlite::OpenFlags::SQLITE_OPEN_NOFOLLOW)?,
            true,
        )
    }

    pub fn demo() -> Result<Self> {
        let mut store = Self::init(Connection::open_in_memory()?, false)?;
        for (title, dest) in [
            ("試作の画面を確認する", "review"),
            ("デモの相談メモを確認", "desk"),
            ("検索を速くする案", "inbox"),
            ("明日の会議を準備する", "inbox"),
            ("会議の議題を３つ書く", "next"),
            ("試作を端末で起動する", "next"),
            ("タスク管理ツールの試作", "projects"),
            ("チームの手順書を整える", "projects"),
            ("画面レビューの返事待ち", "waiting"),
            ("キー設定を見直してみる", "someday"),
            ("キーバインドの早見表", "reference"),
            ("試作の動作確認メモ", "reference"),
        ] {
            let body = match title {
                "検索を速くする案" => {
                    "## メモ\n一覧の絞り込みを試す。\n\n- 題名で検索\n- 本文でも検索\n\n資料: https://example.invalid/search"
                }
                "試作の動作確認メモ" => {
                    "## 確認手順\n\n1. デモを起動\n2. Inboxを開く\n3. 項目を作る\n\n```sh\nlazygtd --demo\n```"
                }
                _ => "",
            };
            let x = store.add_with_body(title, body, "demo")?;
            if dest != "inbox" {
                let x = store.move_to(x.id, x.revision, dest, "demo")?;
                if x.id == 6 || x.id == 9 {
                    let x = store.assign(x.id, x.revision, "agent", "demo")?;
                    store.instruct(
                        x.id,
                        x.revision,
                        "再現手順を整理して、次の確認点を報告する。",
                        "demo",
                    )?;
                }
            }
        }
        Ok(store)
    }

    pub fn rich_demo() -> Result<Self> {
        let mut store = Self::demo()?;
        let project = store.show(7)?;
        let project = store.change_meta(
            7,
            project.revision,
            planning::PlanField::Outcome,
            "チームでデモの流れを確認できる",
            "demo",
        )?;
        let project = store.add_step(
            7,
            project.revision,
            "タスクの入力を試す",
            "## 条件\n- 本文を2行にする\n- 入力結果を確認する",
            "demo",
        )?;
        store.add_step(
            7,
            project.revision,
            "模擬エージェントへの依頼を試す",
            "次の行動に繰り上がったら、詳細から指示を送る。",
            "demo",
        )?;
        let next = store.show(3)?;
        let today = store.today()?;
        store.change_meta(3, next.revision, planning::PlanField::Due, &today, "demo")?;
        let waiting = store.show(9)?;
        let waiting = store.change_meta(
            9,
            waiting.revision,
            planning::PlanField::Who,
            "架空のレビュアー",
            "demo",
        )?;
        let waiting = store.change_meta(
            9,
            waiting.revision,
            planning::PlanField::FollowUp,
            &today,
            "demo",
        )?;
        store.change_meta(
            9,
            waiting.revision,
            planning::PlanField::Watch,
            "https://example.invalid/reviews/1 https://example.invalid/reviews/2",
            "demo",
        )?;
        let desk = store.show(2)?;
        let desk = store.send_message(
            2,
            desk.revision,
            "レビューのときに見せる手順を整理して",
            "demo",
        )?;
        let run = store.start_run(2, desk.revision, "demo")?;
        store.finish_run(
            run,
            "review",
            "デモの返事: まず入力を試し、次の行動に移してみてください。",
        )?;
        let waiting = store.show(8)?;
        store.send_message(8, waiting.revision, "デモ: 手順書の案を出して", "demo")?;
        for (dest, titles) in [
            (
                "review",
                [
                    "画面構成の案を選ぶ",
                    "テスト結果を確認する",
                    "変更点をメモする",
                    "次の試作の範囲を決める",
                ],
            ),
            (
                "desk",
                [
                    "返信の表示を確認する",
                    "質問の種類を整理する",
                    "作業の背景を共有する",
                    "確認したいことをまとめる",
                ],
            ),
            (
                "inbox",
                [
                    "作業ログを読みやすくする案",
                    "あとで調べたい資料",
                    "表示が長いときに横幅と折り返しを見直すためのかなり長い題名の例",
                    "試作の使い方を見直す",
                ],
            ),
            (
                "next",
                [
                    "READMEの例を見直す",
                    "一覧のキーを比較する",
                    "本文に表を書いてみる",
                    "取り込みの方針をまとめる",
                ],
            ),
            (
                "projects",
                [
                    "サンプル手順書を仕上げる",
                    "入力画面の使いやすさを確認する",
                    "タスク分類を試す",
                    "端末での動作をまとめる",
                ],
            ),
            (
                "waiting",
                [
                    "テストの返事を待つ",
                    "レビューの確認を待つ",
                    "資料の見直しを待つ",
                    "モック成果物の確認を待つ",
                ],
            ),
            (
                "someday",
                [
                    "テーマの切り替えを試す",
                    "ショートカットを見直す",
                    "別の端末で表示を試す",
                    "読書メモを整理する",
                ],
            ),
            (
                "reference",
                [
                    "仕様の確認メモ",
                    "模擬エージェントの見方",
                    "テストの表示例",
                    "デモの操作手順",
                ],
            ),
        ] {
            for title in titles {
                let body = if title == "テストの表示例" {
                    "## 日本語の表\n\n| 項目 | 結果 |\n| --- | --- |\n| 日本語の列 | 問題なし |\n| 記号と英字 | OK |\n\n参考: [資料](https://example.invalid/table)"
                } else {
                    ""
                };
                let item = store.add_with_body(title, body, "demo")?;
                if dest != "inbox" {
                    store.move_to(item.id, item.revision, dest, "demo")?;
                }
            }
        }
        questions::seed_demo(&store.0, questions::now())
            .map_err(|_| StoreError::Invalid("could not seed demo questions"))?;
        Ok(store)
    }

    pub fn recover_agent(&mut self, id: i64, run: i64) -> Result<()> {
        let latest = self
            .latest_run(id)?
            .ok_or(StoreError::Invalid("no run on this task"))?;
        if latest.id != run || latest.state != "running" {
            return Err(StoreError::Invalid(
                "run is not the active run on this task",
            ));
        }
        self.finish_run(
            run,
            "failed",
            "実行を停止済みとして回収しました。外部で完了したかは確認できません。",
        )
    }

    pub fn import_question(
        &mut self,
        new: &questions::ImportQuestion,
        now: i64,
    ) -> questions::Result<i64> {
        questions::register(&self.0, new, now)
    }

    fn init(mut c: Connection, file: bool) -> Result<Self> {
        if rusqlite::version_number() < 3_051_003 {
            return Err(StoreError::Invalid("SQLite 3.51.3 or newer is required"));
        }
        c.busy_timeout(Duration::from_secs(5))?;
        c.pragma_update(None, "foreign_keys", true)?;
        c.pragma_update(None, "trusted_schema", false)?;
        let tx = c.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let app: i64 = tx.pragma_query_value(None, "application_id", |r| r.get(0))?;
        let ver: i64 = tx.pragma_query_value(None, "user_version", |r| r.get(0))?;
        match (app, ver) {
            (0, 0) => {
                let entries: i64 =
                    tx.query_row("SELECT count(*) FROM sqlite_schema", [], |r| r.get(0))?;
                if entries != 0 {
                    return Err(StoreError::Invalid(
                        "refusing to modify a database not created by lazygtd",
                    ));
                }
                tx.execute_batch(SCHEMA)?;
                tx.pragma_update(None, "application_id", APP_ID)?;
            }
            (APP_ID, 1..=7) => {}
            _ => {
                return Err(StoreError::Invalid(
                    "database is not a supported lazygtd schema",
                ));
            }
        }
        if ver < 2 {
            tx.execute_batch(V2)?;
        }
        if ver < 3 {
            tx.execute_batch(V3)?;
        }
        if ver < 4 {
            tx.execute_batch(talk::SCHEMA)?;
        }
        if ver < 5 {
            tx.execute_batch(planning::SCHEMA)?;
            tx.execute_batch(questions::SCHEMA)?;
        }
        if ver < 6 {
            tx.execute_batch(crate::sessions::SCHEMA)?;
        }
        if ver < 7 {
            tx.execute_batch(V7)?;
            tx.pragma_update(None, "user_version", 7)?;
        }
        tx.commit()?;
        if file {
            let mode: String = c.query_row("PRAGMA journal_mode=WAL", [], |r| r.get(0))?;
            if !mode.eq_ignore_ascii_case("wal") {
                return Err(StoreError::Invalid(
                    "WAL unavailable; use a local filesystem",
                ));
            }
            c.pragma_update(None, "synchronous", "FULL")?;
        }
        Ok(Self(c))
    }

    fn mutate(&mut self, id: i64, rev: i64, change: Change<'_>, actor: &str) -> Result<Item> {
        let actor = self::actor(actor)?;
        let tx = self
            .0
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let mut item = show(&tx, id)?;
        if item.revision != rev {
            return Err(StoreError::Stale {
                id,
                expected: rev,
                current: item.revision,
            });
        }
        open_item(&item)?;
        require_idle(&tx, id)?;
        let dest = match change {
            Change::Move(box_id) => Some(box_id),
            Change::Done | Change::Archive => None,
        };
        planning::before_item_change(&tx, id, dest)?;
        let kind = match change {
            Change::Move(box_id) => {
                if !BOX_IDS.contains(&box_id) {
                    return Err(StoreError::Invalid("unknown box"));
                }
                if item.box_id == box_id && !item.agent_review {
                    return Ok(item);
                }
                item.box_id = box_id.into();
                "move"
            }
            Change::Done => {
                item.done = true;
                "done"
            }
            Change::Archive => {
                item.archived = true;
                "archive"
            }
        };
        item.revision = item
            .revision
            .checked_add(1)
            .ok_or(StoreError::Invalid("revision exhausted"))?;
        let count = tx.execute(
            "UPDATE item SET box_id=?1, done=?2, archived=?6, revision=?3,details=json_remove(details,'$.agent_review') WHERE id=?4 AND revision=?5",
            params![item.box_id, item.done, item.revision, id, rev, item.archived],
        )?;
        if count != 1 {
            return Err(StoreError::Invalid(
                "item update did not affect exactly one row",
            ));
        }
        record(&tx, &item, kind, actor)?;
        planning::on_item_changed(&tx, id)?;
        let item = show(&tx, id)?;
        tx.commit()?;
        Ok(item)
    }
}

fn validate<'a>(value: &'a str, max: usize, error: &'static str) -> Result<&'a str> {
    let value = value.trim();
    if value.is_empty() || value.chars().count() > max || value.chars().any(char::is_control) {
        return Err(StoreError::Invalid(error));
    }
    Ok(value)
}

fn checked_body(body: &str) -> Result<String> {
    if body.len() > 32_000
        || body
            .chars()
            .any(|c| c.is_control() && !matches!(c, '\t' | '\n' | '\r'))
    {
        return Err(StoreError::Invalid(
            "body must be plain UTF-8 text up to 32000 bytes",
        ));
    }
    Ok(body.replace("\r\n", "\n").replace('\r', "\n"))
}

fn row(r: &rusqlite::Row<'_>) -> rusqlite::Result<Item> {
    let json: String = r.get(10)?;
    let planning = serde_json::from_str(&json).map_err(|e| {
        rusqlite::Error::FromSqlConversionFailure(10, rusqlite::types::Type::Text, Box::new(e))
    })?;
    Ok(Item {
        id: r.get(0)?,
        revision: r.get(1)?,
        title: r.get(2)?,
        box_id: r.get(3)?,
        done: r.get(4)?,
        archived: r.get(14)?,
        assignee: r.get(5)?,
        instructions: r.get(6)?,
        body: r.get(7)?,
        pending_message: r.get(8)?,
        unread_reply: r.get(9)?,
        planning,
        project_id: r.get(11)?,
        agent_review: r.get(12)?,
        agent_active: r.get(13)?,
    })
}

fn show(c: &Connection, id: i64) -> Result<Item> {
    c.query_row(concat!(select_item!(), " WHERE id=?"), [id], row)
        .optional()?
        .ok_or(StoreError::Missing(id))
}

fn items(c: &Connection, include_archived: bool) -> Result<Vec<Item>> {
    let sql = if include_archived {
        concat!(select_item!(), " ORDER BY id")
    } else {
        concat!(select_item!(), " WHERE archived=0 ORDER BY id")
    };
    let mut stmt = c.prepare(sql)?;
    Ok(stmt
        .query_map([], row)?
        .collect::<rusqlite::Result<Vec<_>>>()?)
}

fn actor(actor: &str) -> Result<&str> {
    validate(
        actor,
        100,
        "actor must be 1–100 characters without controls",
    )
}

fn open_item(item: &Item) -> Result<()> {
    if item.archived {
        return Err(StoreError::Invalid("item is archived; reopen it first"));
    }
    if item.done {
        return Err(StoreError::Invalid("item is already done"));
    }
    Ok(())
}

/// Record the caller's event for a change made by a planning rule, unless the
/// revision did not move or the rule already recorded an event for it.
fn audit(tx: &Connection, id: i64, before: i64, kind: &str, actor: &str) -> Result<()> {
    tx.execute(
        "INSERT INTO event(item_id,revision,actor,kind,box_id,done)
         SELECT id,revision,?,?,box_id,done FROM item
         WHERE id=? AND revision!=?
           AND NOT EXISTS(SELECT 1 FROM event e WHERE e.item_id=item.id AND e.revision=item.revision)",
        params![actor, kind, id, before],
    )?;
    Ok(())
}

fn record(tx: &Connection, item: &Item, kind: &str, actor: &str) -> Result<()> {
    tx.execute(
        "INSERT INTO event (item_id,revision,actor,kind,box_id,done) VALUES (?,?,?,?,?,?)",
        params![item.id, item.revision, actor, kind, item.box_id, item.done],
    )?;
    Ok(())
}

fn require_idle(tx: &Connection, id: i64) -> Result<()> {
    if tx.query_row(
        "SELECT EXISTS(SELECT 1 FROM agent_run WHERE item_id=? AND state='running')",
        [id],
        |r| r.get::<_, bool>(0),
    )? {
        return Err(StoreError::Invalid("an agent run is active for this task"));
    }
    Ok(())
}

fn current(tx: &Connection, id: i64, rev: i64) -> Result<Item> {
    let item = show(tx, id)?;
    if item.revision != rev {
        return Err(StoreError::Stale {
            id,
            expected: rev,
            current: item.revision,
        });
    }
    open_item(&item)?;
    require_idle(tx, id)?;
    Ok(item)
}

fn task_change(tx: &Connection, item: &Item, kind: &str) -> Result<()> {
    tx.execute(
        "INSERT INTO task_change(item_id,revision,kind) VALUES (?,?,?)",
        params![item.id, item.revision, kind],
    )?;
    Ok(())
}

fn agent_prompt(c: &Connection, item: &Item) -> Result<String> {
    if !item.pending_message {
        return Ok(format!("Task: {}\n\n{}", item.title, item.instructions));
    }
    let mut q=c.prepare("SELECT body FROM task_message WHERE item_id=? AND role='user' AND run_id IS NULL ORDER BY id")?;
    let mut prompt = format!("Task: {}\n\n{}", item.title, item.instructions);
    if !item.body.is_empty() {
        prompt.push_str(&format!("\n\nTask notes (context):\n{}", item.body));
    }
    let mut context = Vec::new();
    let mut budget = 24_000_usize;
    for m in talk::list(c, item.id)?
        .into_iter()
        .rev()
        // Claimed instructions and every agent reply; queued instructions follow below.
        .filter(|m| m.run_id.is_some() || m.role == "agent")
    {
        if m.body.len() > budget {
            break;
        }
        budget -= m.body.len();
        context.push(format!("{}: {}", m.role, m.body));
    }
    if !context.is_empty() {
        context.reverse();
        prompt.push_str("\n\nEarlier conversation (context only):\n");
        prompt.push_str(&context.join("\n"));
    }
    for body in q.query_map([item.id], |r| r.get::<_, String>(0))? {
        prompt.push_str("\n\nNew instruction:\n");
        prompt.push_str(&body?);
        if prompt.len() > 100_000 {
            return Err(StoreError::Invalid(
                "message batch too large to run; review queued instructions",
            ));
        }
    }
    Ok(prompt)
}

impl Store for Sqlite {
    fn list(&self) -> Result<Vec<Item>> {
        items(&self.0, false)
    }
    fn list_all(&self) -> Result<Vec<Item>> {
        items(&self.0, true)
    }
    fn show(&self, id: i64) -> Result<Item> {
        show(&self.0, id)
    }
    fn create(&mut self, new: &NewItem<'_>) -> Result<Item> {
        let body = checked_body(new.body)?;
        let title = validate(
            new.title,
            500,
            "title must be 1–500 characters without controls",
        )?;
        let actor = actor(new.actor)?;
        let box_id = new.box_id.unwrap_or("inbox");
        if !BOX_IDS.contains(&box_id) {
            return Err(StoreError::Invalid("unknown box"));
        }
        let assignee = new.assignee.unwrap_or("me");
        if !["me", "agent"].contains(&assignee) {
            return Err(StoreError::Invalid("assignee must be me or agent"));
        }
        let mut plan = serde_json::Map::new();
        for (field, value) in &new.meta {
            planning::check_box(*field, box_id)?;
            let value = planning::normalize(*field, value)?;
            if plan.insert(field.key().into(), value).is_some() {
                return Err(StoreError::Invalid("give each planning field at most once"));
            }
        }
        plan.retain(|_, value| !value.is_null() && value != &serde_json::json!([]));
        let tx = self
            .0
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        // A move into a box records the day it started there; creating straight
        // into a box does the same unless the caller gives that day.
        if box_id != "inbox" && !plan.contains_key("since") {
            plan.insert("since".into(), planning::today(&tx)?.into());
        }
        let mut details = serde_json::json!({ "body": body });
        if !plan.is_empty() {
            details["planning"] = plan.into();
        }
        tx.execute(
            "INSERT INTO item(revision,title,box_id,done,assignee,details) VALUES (1,?,?,0,?,json(?))",
            params![title, box_id, assignee, details.to_string()],
        )?;
        let item = show(&tx, tx.last_insert_rowid())?;
        record(&tx, &item, "add", actor)?;
        tx.commit()?;
        Ok(item)
    }
    fn edit_body(&mut self, id: i64, rev: i64, body: &str, actor: &str) -> Result<Item> {
        let body = checked_body(body)?;
        let actor = self::actor(actor)?;
        let tx = self
            .0
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let item = current(&tx, id, rev)?;
        if item.body == body {
            return Ok(item);
        }
        tx.execute(
            "UPDATE item SET details=json_set(details,'$.body',?), revision=revision+1 WHERE id=?",
            params![body, id],
        )?;
        let item = show(&tx, id)?;
        task_change(&tx, &item, "edit-body")?;
        record(&tx, &item, "edit-body", actor)?;
        tx.commit()?;
        Ok(item)
    }
    fn retitle(&mut self, id: i64, rev: i64, title: &str, actor: &str) -> Result<Item> {
        let title = validate(
            title,
            500,
            "title must be 1–500 characters without controls",
        )?;
        let actor = self::actor(actor)?;
        let tx = self
            .0
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let item = current(&tx, id, rev)?;
        if item.title == title {
            return Ok(item);
        }
        tx.execute(
            "UPDATE item SET title=?, revision=revision+1 WHERE id=?",
            params![title, id],
        )?;
        let item = show(&tx, id)?;
        record(&tx, &item, "retitle", actor)?;
        tx.commit()?;
        Ok(item)
    }
    fn move_to(&mut self, id: i64, rev: i64, dest: &str, actor: &str) -> Result<Item> {
        self.mutate(id, rev, Change::Move(dest), actor)
    }
    fn done(&mut self, id: i64, rev: i64, actor: &str) -> Result<Item> {
        self.mutate(id, rev, Change::Done, actor)
    }
    fn archive(&mut self, id: i64, rev: i64, actor: &str) -> Result<Item> {
        self.mutate(id, rev, Change::Archive, actor)
    }
    fn reopen(&mut self, id: i64, rev: i64, actor: &str) -> Result<Item> {
        let actor = self::actor(actor)?;
        let tx = self
            .0
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let item = show(&tx, id)?;
        if item.revision != rev {
            return Err(StoreError::Stale {
                id,
                expected: rev,
                current: item.revision,
            });
        }
        if !item.done && !item.archived {
            return Err(StoreError::Invalid("item is neither done nor archived"));
        }
        require_idle(&tx, id)?;
        if let Some(project) = item.project_id {
            let project = show(&tx, project)?;
            if project.done || project.archived {
                return Err(StoreError::Invalid("reopen the step's project first"));
            }
        }
        tx.execute(
            "UPDATE item SET done=0, archived=0, revision=revision+1 WHERE id=?",
            [id],
        )?;
        let item = show(&tx, id)?;
        record(&tx, &item, "reopen", actor)?;
        planning::on_item_changed(&tx, id)?;
        let item = show(&tx, id)?;
        tx.commit()?;
        Ok(item)
    }
    fn assign(&mut self, id: i64, rev: i64, who: &str, actor: &str) -> Result<Item> {
        if !["me", "agent"].contains(&who) {
            return Err(StoreError::Invalid("assignee must be me or agent"));
        }
        let actor = self::actor(actor)?;
        let tx = self
            .0
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let item = current(&tx, id, rev)?;
        if item.assignee == who {
            return Ok(item);
        }
        tx.execute(
            "UPDATE item SET assignee=?,revision=revision+1 WHERE id=?",
            params![who, id],
        )?;
        let item = show(&tx, id)?;
        task_change(&tx, &item, "assign")?;
        record(&tx, &item, "assign", actor)?;
        tx.commit()?;
        Ok(item)
    }

    fn instruct(&mut self, id: i64, rev: i64, text: &str, actor: &str) -> Result<Item> {
        let text = text.trim();
        if text.len() > 16000
            || text.is_empty()
            || text
                .chars()
                .any(|c| c.is_control() && !matches!(c, '\n' | '\t' | '\r'))
        {
            return Err(StoreError::Invalid(
                "instructions must be 1–16000 bytes of plain text",
            ));
        }
        let actor = self::actor(actor)?;
        let tx = self
            .0
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let item = current(&tx, id, rev)?;
        if item.instructions == text {
            return Ok(item);
        }
        tx.execute(
            "UPDATE item SET instructions=?,revision=revision+1 WHERE id=?",
            params![text, id],
        )?;
        let item = show(&tx, id)?;
        task_change(&tx, &item, "instruct")?;
        record(&tx, &item, "instruct", actor)?;
        tx.commit()?;
        Ok(item)
    }

    fn conversation(&self, id: i64) -> Result<Vec<talk::Message>> {
        Ok(talk::list(&self.0, id)?)
    }
    fn send_message(&mut self, id: i64, rev: i64, text: &str, actor: &str) -> Result<Item> {
        let text = checked_body(text.trim())?;
        if text.is_empty() || text.len() > 16_000 {
            return Err(StoreError::Invalid("instruction must be 1–16000 bytes"));
        }
        let actor = self::actor(actor)?;
        let tx = self
            .0
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        current(&tx, id, rev)?;
        let size:i64=tx.query_row("SELECT coalesce(sum(length(CAST(body AS BLOB))),0) FROM task_message WHERE item_id=? AND role='user' AND run_id IS NULL",[id],|r|r.get(0))?;
        if size + text.len() as i64 > 20_000 {
            return Err(StoreError::Invalid(
                "too much queued text; run the pending instructions first",
            ));
        }
        talk::user(&tx, id, &text)?;
        tx.execute("UPDATE item SET revision=revision+1 WHERE id=?", [id])?;
        let item = show(&tx, id)?;
        record(&tx, &item, "message", actor)?;
        tx.commit()?;
        Ok(item)
    }
    fn reply(&mut self, id: i64, rev: i64, text: &str, actor: &str) -> Result<Item> {
        let text = checked_body(text.trim())?;
        if text.is_empty() {
            return Err(StoreError::Invalid("reply must not be empty"));
        }
        let actor = self::actor(actor)?;
        let tx = self
            .0
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        current(&tx, id, rev)?;
        talk::reply(&tx, id, &text)?;
        tx.execute("UPDATE item SET revision=revision+1 WHERE id=?", [id])?;
        let item = show(&tx, id)?;
        record(&tx, &item, "reply", actor)?;
        tx.commit()?;
        Ok(item)
    }
    fn mark_read(&mut self, id: i64, through: i64) -> Result<()> {
        Ok(talk::mark_read(&self.0, id, through)?)
    }
    fn agent_prompt(&self, id: i64) -> Result<String> {
        agent_prompt(&self.0, &show(&self.0, id)?)
    }
    fn run_input(&self, run: i64) -> Result<String> {
        Ok(self
            .0
            .query_row("SELECT input FROM agent_run WHERE id=?", [run], |r| {
                r.get(0)
            })?)
    }

    fn calendar_action(&mut self, id: i64, rev: i64, actor: &str) -> Result<Item> {
        let actor = self::actor(actor)?;
        let tx = self
            .0
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        planning::mark_calendar(&tx, id, rev, actor)?;
        audit(&tx, id, rev, "plan-meta", actor)?;
        let item = show(&tx, id)?;
        tx.commit()?;
        Ok(item)
    }
    fn watch_resolve(&mut self, id: i64, rev: i64, closed: &[&str], actor: &str) -> Result<Item> {
        let actor = self::actor(actor)?;
        let tx = self
            .0
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let item = current(&tx, id, rev)?;
        let Some(urls) = item.planning.watch.as_deref().filter(|s| !s.is_empty()) else {
            return Err(StoreError::Invalid("no watch URLs on this item"));
        };
        if item.done || item.box_id != "waiting" {
            return Err(StoreError::Invalid(
                "watch only applies to active items in the waiting box",
            ));
        }
        if urls.split_whitespace().all(|url| closed.contains(&url)) {
            tx.execute(
                "UPDATE item SET box_id='next',revision=revision+1 WHERE id=?",
                [id],
            )?;
            let item = show(&tx, id)?;
            record(&tx, &item, "move", actor)?;
            planning::on_item_changed(&tx, id)?;
        }
        let item = show(&tx, id)?;
        tx.commit()?;
        Ok(item)
    }

    fn today(&self) -> Result<String> {
        planning::today(&self.0)
    }
    fn steps(&self, id: i64) -> Result<Vec<planning::Step>> {
        planning::steps(&self.0, id)
    }
    fn change_meta(
        &mut self,
        id: i64,
        rev: i64,
        field: planning::PlanField,
        value: &str,
        actor: &str,
    ) -> Result<Item> {
        let actor = self::actor(actor)?;
        let tx = self
            .0
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        planning::change_meta(&tx, id, rev, field, value)?;
        audit(&tx, id, rev, "plan-meta", actor)?;
        let item = show(&tx, id)?;
        tx.commit()?;
        Ok(item)
    }
    fn add_step(
        &mut self,
        project: i64,
        rev: i64,
        title: &str,
        body: &str,
        actor: &str,
    ) -> Result<Item> {
        let title = validate(
            title,
            500,
            "title must be 1–500 characters without controls",
        )?;
        let body = checked_body(body)?;
        let actor = self::actor(actor)?;
        let tx = self
            .0
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        tx.execute("INSERT INTO item(revision,title,box_id,done,details) VALUES (1,?,'someday',0,json_object('body',?))",params![title,body])?;
        let step = show(&tx, tx.last_insert_rowid())?;
        record(&tx, &step, "add", actor)?;
        planning::attach(&tx, project, rev, step.id, step.revision)?;
        audit(&tx, project, rev, "step-attach", actor)?;
        let item = show(&tx, project)?;
        tx.commit()?;
        Ok(item)
    }
    fn attach(
        &mut self,
        project: i64,
        rev: i64,
        step: i64,
        step_rev: i64,
        actor: &str,
    ) -> Result<Item> {
        let actor = self::actor(actor)?;
        let tx = self
            .0
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        planning::attach(&tx, project, rev, step, step_rev)?;
        audit(&tx, project, rev, "step-attach", actor)?;
        let item = show(&tx, project)?;
        tx.commit()?;
        Ok(item)
    }
    fn detach(
        &mut self,
        project: i64,
        rev: i64,
        step: i64,
        step_rev: i64,
        actor: &str,
    ) -> Result<Item> {
        let actor = self::actor(actor)?;
        let tx = self
            .0
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        planning::detach(&tx, project, rev, step, step_rev)?;
        audit(&tx, project, rev, "step-detach", actor)?;
        let item = show(&tx, project)?;
        tx.commit()?;
        Ok(item)
    }
    fn reorder(&mut self, project: i64, rev: i64, order: &[i64], actor: &str) -> Result<Item> {
        let actor = self::actor(actor)?;
        let tx = self
            .0
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        planning::reorder(&tx, project, rev, order)?;
        audit(&tx, project, rev, "step-reorder", actor)?;
        let item = show(&tx, project)?;
        tx.commit()?;
        Ok(item)
    }
    fn questions(&self, now: i64) -> questions::Result<Vec<questions::Question>> {
        questions::list(&self.0, now)
    }
    fn answer(
        &mut self,
        id: i64,
        answer: questions::Answer<'_>,
        now: i64,
    ) -> questions::Result<()> {
        questions::answer(&self.0, id, answer, now)
    }
    fn latest_run(&self, id: i64) -> Result<Option<AgentRun>> {
        Ok(self.0.query_row("SELECT a.id,a.item_revision,a.provider,a.state,a.activity,a.result,a.started_at,a.finished_at,s.native_session FROM agent_run a LEFT JOIN agent_session_run s ON s.run_id=a.id WHERE a.item_id=? ORDER BY a.id DESC LIMIT 1", [id], |r| Ok(AgentRun {
            id:r.get(0)?, item_revision:r.get(1)?, provider:r.get(2)?, state:r.get(3)?, activity:r.get(4)?,
            result:r.get(5)?, started_at:r.get(6)?, finished_at:r.get(7)?, session_id:r.get(8)?,
        })).optional()?)
    }

    fn check_session(
        &self,
        id: i64,
        provider: &str,
        scope: crate::sessions::Request<'_>,
    ) -> Result<()> {
        crate::sessions::check(&self.0, id, provider, scope)
    }

    fn start_run_scoped(
        &mut self,
        id: i64,
        rev: i64,
        provider: &str,
        scope: Option<crate::sessions::Request<'_>>,
    ) -> Result<i64> {
        let tx = self
            .0
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let item = current(&tx, id, rev)?;
        if !item.pending_message && (item.assignee != "agent" || item.instructions.is_empty()) {
            return Err(StoreError::Invalid(
                "assign to agent and save instructions first",
            ));
        }
        let input = agent_prompt(&tx, &item)?;
        tx.execute("INSERT INTO agent_run(item_id,item_revision,provider,input,state) VALUES(?,?,?,?,'running')",
            params![id,rev,provider,input])?;
        let run = tx.last_insert_rowid();
        if let Some(scope) = scope {
            crate::sessions::reserve(&tx, run, id, provider, scope)?;
        }
        talk::claim(&tx, id, run)?;
        tx.execute(
            "UPDATE item SET details=json_remove(details,'$.agent_review') WHERE id=?",
            [id],
        )?;
        tx.commit()?;
        Ok(run)
    }

    fn progress(&mut self, run: i64, event: &str) -> Result<()> {
        self.0.execute(
            "UPDATE agent_run SET activity=? WHERE id=? AND state='running'",
            params![event, run],
        )?;
        Ok(())
    }

    fn finish_run_scoped(
        &mut self,
        run: i64,
        state: &str,
        result: &str,
        native_session: Option<&str>,
    ) -> Result<()> {
        if !["review", "failed"].contains(&state) {
            return Err(StoreError::Invalid("invalid terminal run status"));
        }
        let tx = self
            .0
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let id: i64 = tx
            .query_row(
                "SELECT item_id FROM agent_run WHERE id=? AND state='running'",
                [run],
                |r| r.get(0),
            )
            .optional()?
            .ok_or(StoreError::Invalid("run is not active"))?;
        crate::sessions::finish(&tx, run, state, native_session)?;
        tx.execute("UPDATE agent_run SET state=?,result=?,finished_at=strftime('%Y-%m-%dT%H:%M:%fZ','now') WHERE id=?", params![state,result,run])?;
        tx.execute(
            "UPDATE item SET assignee='me',box_id=CASE WHEN box_id='projects' THEN 'projects' ELSE 'review' END,revision=revision+1,
                    details=json_set(details,'$.agent_review',json('true')) WHERE id=?",
            [id],
        )?;
        let item = show(&tx, id)?;
        task_change(&tx, &item, "handback")?;
        planning::on_item_changed(&tx, id)?;
        let in_conversation: bool = tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM task_message WHERE run_id=? AND role='user')",
            [run],
            |r| r.get(0),
        )?;
        if in_conversation {
            talk::agent(&tx, id, run, result)?;
        }
        tx.commit()?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn v5_migration_preserves_tasks_and_agent_runs() {
        let mut old = Sqlite::demo().unwrap();
        let item = old
            .add_with_body("migration fixture", "local body", "user")
            .unwrap();
        let item = old
            .send_message(item.id, item.revision, "queued question", "user")
            .unwrap();
        let run = old.start_run(item.id, item.revision, "demo").unwrap();
        old.finish_run(run, "review", "local reply").unwrap();
        old.0.execute_batch("DROP TABLE agent_session_run; DROP TABLE agent_session_binding; ALTER TABLE item DROP COLUMN archived; PRAGMA user_version=5;").unwrap();
        let migrated = Sqlite::init(old.0, false).unwrap();
        assert_eq!(migrated.show(item.id).unwrap().body, "local body");
        let prior = migrated.latest_run(item.id).unwrap().unwrap();
        assert_eq!(prior.result, "local reply");
        assert!(prior.session_id.is_none());
        assert_eq!(migrated.conversation(item.id).unwrap().len(), 2);
    }

    #[test]
    fn injected_event_failure_rolls_back_mutation_and_revision() {
        let mut store = Sqlite::demo().unwrap();
        let a = store.add_with_body("atomic example", "", "user").unwrap();
        store.0.execute_batch("CREATE TEMP TRIGGER fail_event BEFORE INSERT ON main.event BEGIN SELECT RAISE(ABORT,'blocked event'); END;").unwrap();
        assert!(store.move_to(a.id, a.revision, "next", "user").is_err());
        assert_eq!(store.show(a.id).unwrap(), a);
        let count: i64 = store
            .0
            .query_row("SELECT count(*) FROM event WHERE item_id=?", [a.id], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(count, 1);
        store.0.execute_batch("DROP TRIGGER fail_event;").unwrap();
        let b = store.move_to(a.id, a.revision, "next", "user").unwrap();
        assert!(matches!(
            store.done(a.id, a.revision, "user"),
            Err(StoreError::Stale { .. })
        ));
        assert_eq!(store.show(a.id).unwrap(), b);
        let events = store
            .0
            .prepare("SELECT revision,kind,actor FROM event WHERE item_id=? ORDER BY revision")
            .unwrap()
            .query_map([a.id], |r| {
                Ok((
                    r.get::<_, i64>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, String>(2)?,
                ))
            })
            .unwrap()
            .collect::<rusqlite::Result<Vec<_>>>()
            .unwrap();
        assert_eq!(
            events,
            vec![
                (1, "add".into(), "user".into()),
                (2, "move".into(), "user".into())
            ]
        );
    }

    #[test]
    fn v1_migrates_existing_items_and_preserves_revision_and_events() {
        let c = Connection::open_in_memory().unwrap();
        c.execute_batch(SCHEMA).unwrap();
        c.pragma_update(None, "application_id", APP_ID).unwrap();
        c.pragma_update(None, "user_version", 1).unwrap();
        c.execute_batch("INSERT INTO item VALUES (10,3,'example','next',0);
        INSERT INTO event(item_id,revision,actor,kind,box_id,done) VALUES(10,3,'user','move','next',0);").unwrap();
        let store = Sqlite::init(c, false).unwrap();
        let item = store.show(10).unwrap();
        assert_eq!(item.revision, 3);
        assert_eq!(item.assignee, "me");
        assert!(item.instructions.is_empty());
        assert_eq!(
            store
                .0
                .query_row("SELECT count(*) FROM event", [], |r| r.get::<_, i64>(0))
                .unwrap(),
            1
        );
    }

    #[test]
    fn v2_migration_preserves_tasks_runs_and_existing_audit() {
        let c = Connection::open_in_memory().unwrap();
        c.execute_batch(SCHEMA).unwrap();
        c.execute_batch(V2).unwrap();
        c.pragma_update(None, "application_id", APP_ID).unwrap();
        c.pragma_update(None, "user_version", 2).unwrap();
        c.execute_batch("INSERT INTO item(id,revision,title,box_id,done,assignee,instructions)
            VALUES(7,2,'example','next',0,'agent','look');
            INSERT INTO task_change VALUES(7,2,'assign');
            INSERT INTO agent_run(item_id,item_revision,provider,input,state) VALUES(7,2,'demo','test','failed');").unwrap();
        let mut store = Sqlite::init(c, false).unwrap();
        let item = store.show(7).unwrap();
        assert_eq!(
            (
                item.revision,
                item.body.as_str(),
                item.instructions.as_str()
            ),
            (2, "", "look")
        );
        assert_eq!(store.latest_run(7).unwrap().unwrap().state, "failed");
        store.0.execute("UPDATE item SET details=json_set(details,'$.experimental',json('[1,2]')) WHERE id=7",[]).unwrap();
        assert!(store.edit_body(7, 1, "stale", "user").is_err());
        let changed = store.edit_body(7, 2, "日本語\r\n- リスト", "user").unwrap();
        assert_eq!(changed.revision, 3);
        assert_eq!(changed.body, "日本語\n- リスト");
        let metadata: String = store
            .0
            .query_row(
                "SELECT json_extract(details,'$.experimental') FROM item WHERE id=7",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(metadata, "[1,2]");
        let changes: i64 = store
            .0
            .query_row(
                "SELECT count(*) FROM task_change WHERE item_id=7",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(changes, 2);
    }

    #[test]
    fn agent_handback_keeps_projects_and_explicit_recovery_matches_id() {
        let mut store = Sqlite::rich_demo().unwrap();
        let p = store.show(7).unwrap();
        let p = store
            .send_message(p.id, p.revision, "Planning review", "user")
            .unwrap();
        let run = store.start_run(p.id, p.revision, "demo").unwrap();
        assert!(store.recover_agent(1, run).is_err());
        assert!(store.show(p.id).unwrap().agent_active);
        store.recover_agent(p.id, run).unwrap();
        let p = store.show(p.id).unwrap();
        assert!(p.agent_review);
        assert!(!p.agent_active);
        assert_eq!(p.box_id, "projects");
        assert_eq!(store.steps(p.id).unwrap().len(), 2);
        assert!(
            !store
                .move_to(p.id, p.revision, "projects", "user")
                .unwrap()
                .agent_review
        );
    }

    fn events(store: &Sqlite, id: i64) -> Vec<(i64, String, String)> {
        store
            .0
            .prepare("SELECT revision,kind,actor FROM event WHERE item_id=? ORDER BY revision")
            .unwrap()
            .query_map([id], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
            .unwrap()
            .collect::<rusqlite::Result<Vec<_>>>()
            .unwrap()
    }

    #[test]
    fn v6_migration_keeps_events_and_accepts_new_kinds() {
        let mut old = Sqlite::demo().unwrap();
        let item = old.add_with_body("v6 fixture", "", "user").unwrap();
        let item = old.move_to(item.id, item.revision, "next", "user").unwrap();
        old.0
            .execute_batch(&format!(
                "ALTER TABLE item DROP COLUMN archived;
                 ALTER TABLE event RENAME TO event_v7;
                 {}
                 INSERT INTO event SELECT * FROM event_v7 WHERE kind IN ('add','move','done');
                 DROP TABLE event_v7;
                 PRAGMA user_version=6;",
                &SCHEMA[SCHEMA.find("CREATE TABLE event").unwrap()..]
            ))
            .unwrap();
        let mut migrated = Sqlite::init(old.0, false).unwrap();
        assert_eq!(
            events(&migrated, item.id),
            [
                (1, "add".into(), "user".into()),
                (2, "move".into(), "user".into())
            ]
        );
        let item = migrated
            .retitle(item.id, item.revision, "renamed", "agent:test")
            .unwrap();
        let item = migrated
            .archive(item.id, item.revision, "agent:test")
            .unwrap();
        assert!(item.archived);
        assert!(migrated.list().unwrap().iter().all(|i| i.id != item.id));
        assert!(migrated.list_all().unwrap().iter().any(|i| i.id == item.id));
    }

    #[test]
    fn create_writes_box_assignee_and_planning_in_one_add_event() {
        let mut store = Sqlite::demo().unwrap();
        let before = store.list_all().unwrap().len();
        let new = NewItem {
            title: "wait for review",
            body: "line\r\nnext",
            box_id: Some("waiting"),
            assignee: Some("agent"),
            meta: vec![
                (planning::PlanField::Who, "someone".into()),
                (planning::PlanField::Since, "2026-01-02".into()),
                (
                    planning::PlanField::Source,
                    "https://example.invalid/thread/1".into(),
                ),
            ],
            actor: "agent:test",
        };
        let item = store.create(&new).unwrap();
        assert_eq!(
            (
                item.revision,
                item.box_id.as_str(),
                item.assignee.as_str(),
                item.body.as_str()
            ),
            (1, "waiting", "agent", "line\nnext")
        );
        assert_eq!(item.planning.who.as_deref(), Some("someone"));
        assert_eq!(item.planning.since.as_deref(), Some("2026-01-02"));
        assert_eq!(
            item.planning.source.as_deref(),
            Some("https://example.invalid/thread/1")
        );
        assert_eq!(
            events(&store, item.id),
            [(1, "add".into(), "agent:test".into())]
        );

        let next = store
            .create(&NewItem {
                title: "plain next",
                box_id: Some("next"),
                actor: "user",
                ..NewItem::default()
            })
            .unwrap();
        assert_eq!(next.planning.since, Some(store.today().unwrap()));

        for bad in [
            NewItem {
                title: "who outside waiting",
                box_id: Some("next"),
                meta: vec![(planning::PlanField::Who, "someone".into())],
                actor: "user",
                ..NewItem::default()
            },
            NewItem {
                title: "bad date",
                meta: vec![(planning::PlanField::Due, "2026-02-30".into())],
                actor: "user",
                ..NewItem::default()
            },
            NewItem {
                title: "twice",
                meta: vec![
                    (planning::PlanField::Due, "2026-02-01".into()),
                    (planning::PlanField::Due, "2026-02-02".into()),
                ],
                actor: "user",
                ..NewItem::default()
            },
            NewItem {
                title: "bad box",
                box_id: Some("nowhere"),
                actor: "user",
                ..NewItem::default()
            },
        ] {
            assert!(store.create(&bad).is_err(), "{}", bad.title);
        }
        // A failure after the insert (here: the audit event) rolls the item back too.
        store.0.execute_batch("CREATE TEMP TRIGGER fail_event BEFORE INSERT ON main.event BEGIN SELECT RAISE(ABORT,'blocked event'); END;").unwrap();
        assert!(store.create(&new).is_err());
        store.0.execute_batch("DROP TRIGGER fail_event;").unwrap();
        assert_eq!(store.list_all().unwrap().len(), before + 2);
    }

    #[test]
    fn retitle_archive_reopen_and_reply_check_revision_and_record_actor() {
        let mut store = Sqlite::demo().unwrap();
        let a = store.add_with_body("first title", "", "user").unwrap();
        assert!(matches!(
            store.retitle(a.id, a.revision + 1, "x", "user"),
            Err(StoreError::Stale { .. })
        ));
        let a = store
            .retitle(a.id, a.revision, "second title", "agent:bot")
            .unwrap();
        assert_eq!((a.title.as_str(), a.revision), ("second title", 2));
        assert_eq!(store.retitle(a.id, 2, "second title", "user").unwrap(), a);
        assert!(store.retitle(a.id, 2, " ", "user").is_err());

        assert!(store.reopen(a.id, a.revision, "user").is_err());
        let a = store
            .reply(a.id, a.revision, "report\n", "agent:bot")
            .unwrap();
        assert!(a.unread_reply);
        assert!(!a.pending_message);
        let talk = store.conversation(a.id).unwrap();
        assert_eq!(
            (
                talk[0].role.as_str(),
                talk[0].body.as_str(),
                talk[0].run_id,
                talk[0].read
            ),
            ("agent", "report", None, false)
        );
        assert!(store.reply(a.id, a.revision, " \n", "agent:bot").is_err());

        assert!(matches!(
            store.archive(a.id, a.revision - 1, "user"),
            Err(StoreError::Stale { .. })
        ));
        let a = store.archive(a.id, a.revision, "agent:bot").unwrap();
        assert!(a.archived && !a.done);
        assert!(store.retitle(a.id, a.revision, "nope", "user").is_err());
        assert!(store.move_to(a.id, a.revision, "next", "user").is_err());
        assert!(store.archive(a.id, a.revision, "user").is_err());
        let a = store.reopen(a.id, a.revision, "agent:bot").unwrap();
        assert!(!a.archived && !a.done);
        assert_eq!(a.box_id, "inbox");
        let a = store.done(a.id, a.revision, "user").unwrap();
        let a = store.reopen(a.id, a.revision, "user").unwrap();
        assert!(!a.done);
        assert_eq!(
            events(&store, a.id),
            [
                (1, "add".into(), "user".into()),
                (2, "retitle".into(), "agent:bot".into()),
                (3, "reply".into(), "agent:bot".into()),
                (4, "archive".into(), "agent:bot".into()),
                (5, "reopen".into(), "agent:bot".into()),
                (6, "done".into(), "user".into()),
                (7, "reopen".into(), "user".into()),
            ]
        );
    }

    #[test]
    fn agent_reply_outside_a_run_is_context_for_the_next_run() {
        let mut store = Sqlite::demo().unwrap();
        let a = store.add_with_body("context", "", "user").unwrap();
        let a = store
            .reply(a.id, a.revision, "earlier finding", "agent")
            .unwrap();
        let a = store
            .send_message(a.id, a.revision, "follow up", "user")
            .unwrap();
        let prompt = store.agent_prompt(a.id).unwrap();
        assert!(prompt.contains("agent: earlier finding"), "{prompt}");
        assert!(prompt.contains("New instruction:\nfollow up"), "{prompt}");
    }

    #[test]
    fn archiving_a_step_promotes_the_next_and_existing_writes_record_actor() {
        let mut store = Sqlite::demo().unwrap();
        let p = store
            .create(&NewItem {
                title: "project",
                box_id: Some("projects"),
                actor: "user",
                ..NewItem::default()
            })
            .unwrap();
        let p = store
            .add_step(p.id, p.revision, "one", "", "agent:bot")
            .unwrap();
        let p = store
            .add_step(p.id, p.revision, "two", "", "agent:bot")
            .unwrap();
        let steps = store.steps(p.id).unwrap();
        assert_eq!(steps[0].state, "next");
        assert_eq!(steps[1].state, "planned");
        let first = store.show(steps[0].id).unwrap();
        // Archiving the project with an open step is refused like done.
        assert!(store.archive(p.id, p.revision, "user").is_err());
        store
            .archive(first.id, first.revision, "agent:bot")
            .unwrap();
        let steps = store.steps(p.id).unwrap();
        assert_eq!(
            steps.iter().map(|s| s.state.as_str()).collect::<Vec<_>>(),
            ["archived", "next"]
        );
        let p = store.show(p.id).unwrap();
        let p = store
            .change_meta(
                p.id,
                p.revision,
                planning::PlanField::Outcome,
                "shipped",
                "agent:bot",
            )
            .unwrap();
        let p = store
            .assign(p.id, p.revision, "agent", "agent:bot")
            .unwrap();
        let p = store
            .instruct(p.id, p.revision, "look", "agent:bot")
            .unwrap();
        let p = store
            .edit_body(p.id, p.revision, "notes", "agent:bot")
            .unwrap();
        let kinds = events(&store, p.id)
            .into_iter()
            .map(|(_, kind, actor)| format!("{kind} {actor}"))
            .collect::<Vec<_>>();
        assert_eq!(
            kinds,
            [
                "add user",
                "step-attach agent:bot",
                "step-attach agent:bot",
                "plan-meta agent:bot",
                "assign agent:bot",
                "instruct agent:bot",
                "edit-body agent:bot",
            ]
        );
        assert_eq!(events(&store, p.id).last().unwrap().0, p.revision);
    }

    #[test]
    fn unrelated_sqlite_is_not_claimed() {
        let c = Connection::open_in_memory().unwrap();
        c.execute("CREATE TABLE other(x TEXT)", []).unwrap();
        assert!(matches!(
            Sqlite::init(c, false),
            Err(StoreError::Invalid(_))
        ));
    }
}
