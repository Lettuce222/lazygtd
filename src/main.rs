mod agent;
mod approval_server;
mod claude_review;
mod cli;
mod db_file;
mod local_files;
mod planning;
mod pr_watch;
mod questions;
mod render;
mod review_agent;
mod sessions;
mod store;
mod talk;

#[cfg(test)]
use store::Sqlite;
use store::{Item, Store, StoreError};

use std::{
    collections::BTreeMap,
    env, io,
    path::PathBuf,
    process::ExitCode,
    sync::mpsc,
    time::{Duration, Instant},
};

use crossterm::{
    event::{
        self, DisableBracketedPaste, EnableBracketedPaste, Event, KeyCode, KeyEvent, KeyEventKind,
        KeyModifiers,
    },
    execute,
};
use ratatui::{
    Frame,
    layout::{Constraint, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Text},
    widgets::{Block, Borders, Clear, List, ListItem, Paragraph, Wrap},
};

const BOXES: [&str; 6] = [
    "Inbox",
    "次の行動",
    "プロジェクト",
    "連絡待ち",
    "いつか",
    "資料",
];
const DISPLAY_BOX_IDS: [&str; BOXES.len()] = [
    "inbox",
    "next",
    "projects",
    "waiting",
    "someday",
    "reference",
];
const INBOX: usize = 0;
const NEXT: usize = 1;

fn display_box(id: &str) -> usize {
    if matches!(id, "review" | "desk") {
        return NEXT;
    }
    DISPLAY_BOX_IDS
        .iter()
        .position(|box_id| *box_id == id)
        .unwrap_or(INBOX)
}

const BG: Color = Color::Rgb(36, 40, 59);
const TEXT: Color = Color::Rgb(192, 202, 245);
const BLUE: Color = Color::Rgb(122, 162, 247);
const GREEN: Color = Color::Rgb(158, 206, 106);
const FOCUS: Style = Style::new().fg(GREEN).add_modifier(Modifier::BOLD);
const SELECTED: Style = Style::new().fg(BG).bg(TEXT);
const BASE: Style = Style::new().fg(TEXT).bg(BG);
const MIN_WIDTH: u16 = 66;
const MIN_HEIGHT: u16 = 27;

#[derive(Default, PartialEq, Debug)]
enum Mode {
    #[default]
    Desk,
    Help,
    Add(String),
    Move(Item),
    Instruct(Item, String),
    EditBody(Item, String),
    Filter(String),
    Compose(Item, String),
    Plan(Item, String),
    Steps(i64, usize),
    StepInput(i64, usize, bool, String),
    Question(i64),
    LiveReview,
    QuestionText(i64, bool, String),
    SendConfirm(Item, String),
    Artifacts,
}

struct Desk {
    entries: [Vec<Item>; BOXES.len()],
    store: Box<dyn Store>,
    demo: bool,
    status: String,
    selected_box: usize,
    selected_row: usize,
    query: String,
    detail_scroll: u16,
    detail_page: u16,
    detail_max: u16,
    mock_due: BTreeMap<i64, Instant>,
    reading: Option<(i64, i64, Instant)>,
    today: String,
    questions: Vec<questions::Question>,
    profile_path: Option<PathBuf>,
    agent_config: Option<agent::Config>,
    pr_watcher: Option<pr_watch::Watcher>,
    review_settings: Option<review_agent::Settings>,
    live_review: Option<review_agent::Notice>,
    review_rx: Option<mpsc::Receiver<review_agent::Notice>>,
    note_visible: bool,
    outgoing: Option<mpsc::Receiver<String>>,
    can_confirm: bool,
    edit_request: Option<Item>,
    question_scroll: u16,
    question_end: u16,
    mode: Mode,
}

impl Desk {
    #[cfg(test)]
    fn demo() -> Self {
        Self::new(Box::new(Sqlite::demo().unwrap()), true).unwrap()
    }

    fn new(store: Box<dyn Store>, demo: bool) -> Result<Self, StoreError> {
        let mut desk = Self {
            entries: std::array::from_fn(|_| Vec::new()),
            store,
            demo,
            status: String::new(),
            selected_box: 0,
            selected_row: 0,
            query: String::new(),
            detail_scroll: 0,
            detail_page: 12,
            detail_max: 0,
            mock_due: BTreeMap::new(),
            reading: None,
            today: String::new(),
            questions: Vec::new(),
            profile_path: None,
            agent_config: None,
            pr_watcher: None,
            review_settings: None,
            live_review: None,
            review_rx: None,
            note_visible: false,
            outgoing: None,
            can_confirm: false,
            edit_request: None,
            question_scroll: 0,
            question_end: u16::MAX,
            mode: Mode::Desk,
        };
        desk.reload()?;
        Ok(desk)
    }

    fn picked(&self) -> Option<&Item> {
        self.entries[self.selected_box].get(self.selected_row)
    }

    fn reload(&mut self) -> Result<(), StoreError> {
        self.today = self.store.today()?;
        let selected = self.picked().map(|item| item.id);
        let query = self.query.to_lowercase();
        let mut entries: [Vec<Item>; BOXES.len()] = std::array::from_fn(|_| Vec::new());
        let waiting_approval = self.live_review.as_ref().map(|notice| notice.item_id);
        for item in self.store.list()?.into_iter().filter(|item| !item.done) {
            let box_id = display_box(&item.box_id);
            let matches = waiting_approval == Some(item.id)
                || query.is_empty()
                || item.title.to_lowercase().contains(&query)
                || item.body.to_lowercase().contains(&query);
            let needs_me = item.box_id == "review"
                || item.agent_review
                || item.unread_reply
                || waiting_approval == Some(item.id)
                || !planning::review_reasons(&item.planning, &item.box_id, &self.today).is_empty();
            // A Next Actions projection must not change the persisted assignee or
            // remove the original project/waiting entry.
            if box_id != NEXT && needs_me && (self.selected_box != NEXT || matches) {
                let mut own = item.clone();
                own.assignee = "me".into();
                entries[NEXT].push(own);
            }
            if item.project_id.is_some() && item.box_id == "someday" {
                continue;
            }
            if box_id != self.selected_box || matches {
                let mut item = item;
                if box_id == NEXT && needs_me {
                    item.assignee = "me".into();
                }
                entries[box_id].push(item);
            }
        }
        if let Some(id) = waiting_approval {
            entries[NEXT].sort_by_key(|item| item.id != id);
        }
        self.entries = entries;
        let rows = &self.entries[self.selected_box];
        self.selected_row = rows
            .iter()
            .position(|i| Some(i.id) == selected)
            .unwrap_or(self.selected_row.min(rows.len().saturating_sub(1)));
        if selected != self.picked().map(|x| x.id) {
            self.detail_scroll = 0;
        }
        Ok(())
    }

    fn result(&mut self, result: Result<Item, StoreError>) {
        self.status = match result {
            Ok(item) => {
                self.query.clear();
                self.detail_scroll = 0;
                if let Err(e) = self.reload() {
                    format!("Saved #{} but refresh failed: {e}", item.id)
                } else {
                    if !item.done {
                        self.selected_box = if item.agent_review {
                            NEXT
                        } else {
                            display_box(&item.box_id)
                        };
                        self.selected_row = self.entries[self.selected_box]
                            .iter()
                            .position(|x| x.id == item.id)
                            .unwrap_or(0);
                    }
                    format!("Saved #{} r{}", item.id, item.revision)
                }
            }
            Err(e) => {
                if matches!(e, StoreError::Stale { .. }) {
                    match self.reload() {
                        Ok(()) => e.to_string(),
                        Err(refresh) => format!("{e}; refresh failed: {refresh}"),
                    }
                } else {
                    e.to_string()
                }
            }
        };
    }

    fn select(&mut self, i: usize) {
        let clear = !self.query.is_empty();
        self.query.clear();
        self.detail_scroll = 0;
        self.reading = None;
        self.selected_box = i % BOXES.len();
        self.selected_row = 0;
        if clear && let Err(e) = self.reload() {
            self.status = e.to_string();
        }
    }

    // Returns true only when the event should terminate the preview.
    fn key(&mut self, key: KeyEvent) -> bool {
        if key.kind != KeyEventKind::Press
            && !(key.kind == KeyEventKind::Repeat
                && matches!(
                    key.code,
                    KeyCode::Char('h' | 'j' | 'k' | 'l')
                        | KeyCode::Up
                        | KeyCode::Down
                        | KeyCode::Left
                        | KeyCode::Right
                ))
        {
            return false;
        }
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
            if self.outgoing.is_none() {
                return true;
            }
            self.status = "エージェント実行が終わってから終了できます".into();
            return false;
        }
        let enter = key.code == KeyCode::Enter;
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let shift = key.modifiers.contains(KeyModifiers::SHIFT);
        let forbidden = key
            .modifiers
            .intersects(KeyModifiers::ALT | KeyModifiers::SUPER);
        let newline = !forbidden
            && ((ctrl && !shift && key.code == KeyCode::Char('j')) || (!ctrl && shift && enter));
        if newline {
            match &mut self.mode {
                Mode::QuestionText(_, _, draft)
                | Mode::StepInput(_, _, false, draft)
                | Mode::Compose(_, draft)
                    if draft.len() < 16000 =>
                {
                    draft.push('\n')
                }
                Mode::Add(draft) | Mode::Instruct(_, draft) | Mode::EditBody(_, draft)
                    if draft.len() < 32_500 =>
                {
                    draft.push('\n')
                }
                _ => {}
            }
            return false;
        }
        if matches!(self.mode, Mode::Desk) && ctrl && !shift && !forbidden {
            match key.code {
                KeyCode::Char('d') => {
                    self.detail_scroll = self
                        .detail_scroll
                        .saturating_add(self.detail_page)
                        .min(self.detail_max)
                }
                KeyCode::Char('u') => {
                    self.detail_scroll = self.detail_scroll.saturating_sub(self.detail_page)
                }
                KeyCode::Char('r') => {
                    self.status = match self.reload() {
                        Ok(()) => "Refreshed".into(),
                        Err(e) => e.to_string(),
                    }
                }
                _ => {}
            }
            return false;
        }
        if key
            .modifiers
            .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT | KeyModifiers::SUPER)
        {
            return false;
        }
        match &mut self.mode {
            Mode::LiveReview => {
                use approval_server::ReviewDecision::*;
                let value = match key.code {
                    KeyCode::Char('y') if self.note_visible => Some(AllowNote),
                    KeyCode::Char('n') | KeyCode::Esc => Some(Deny),
                    KeyCode::Char('c') => Some(CancelTurn),
                    _ => None,
                };
                if let Some(value) = value {
                    let sent = self
                        .live_review
                        .take()
                        .is_some_and(|notice| notice.decide(value));
                    self.status = if sent {
                        "今回の要求への判断を送信します".into()
                    } else {
                        "要求は失効しました".into()
                    };
                    self.note_visible = false;
                    self.mode = Mode::Desk;
                    let _ = self.reload();
                }
            }
            Mode::Artifacts => {
                if matches!(key.code, KeyCode::Esc | KeyCode::Char('q')) {
                    self.mode = Mode::Desk;
                }
            }
            Mode::Help => {
                if matches!(key.code, KeyCode::Esc | KeyCode::Char('q' | '?')) {
                    self.mode = Mode::Desk;
                }
            }
            Mode::Add(draft) => match key.code {
                KeyCode::Esc => self.mode = Mode::Desk,
                KeyCode::Enter if !draft.trim().is_empty() => {
                    let (title, body) = draft.split_once('\n').unwrap_or((draft, ""));
                    let result = self.store.add_with_body(title, body, "user");
                    if result.is_ok() {
                        self.mode = Mode::Desk;
                    }
                    self.result(result);
                }
                KeyCode::Backspace => {
                    draft.pop();
                }
                KeyCode::Char(c)
                    if !c.is_control()
                        && draft.len() + c.len_utf8() <= 32_500
                        && !key.modifiers.intersects(
                            KeyModifiers::CONTROL | KeyModifiers::ALT | KeyModifiers::SUPER,
                        ) =>
                {
                    draft.push(c);
                }
                _ => {}
            },
            Mode::Instruct(item, draft) => match key.code {
                KeyCode::Esc => self.mode = Mode::Desk,
                KeyCode::Enter if !draft.trim().is_empty() => {
                    let result = self.store.instruct(item.id, item.revision, draft);
                    if result.is_ok() {
                        self.mode = Mode::Desk;
                    }
                    self.result(result);
                }
                KeyCode::Backspace => {
                    draft.pop();
                }
                KeyCode::Char(c) if !c.is_control() && draft.len() < 16000 => draft.push(c),
                _ => {}
            },
            Mode::EditBody(item, draft) => match key.code {
                KeyCode::Esc => self.mode = Mode::Desk,
                KeyCode::Enter => {
                    let result = self.store.edit_body(item.id, item.revision, draft);
                    if result.is_ok() {
                        self.mode = Mode::Desk;
                    }
                    self.result(result);
                }
                KeyCode::Backspace => {
                    draft.pop();
                }
                KeyCode::Char(c) if !c.is_control() && draft.len() + c.len_utf8() <= 32_000 => {
                    draft.push(c)
                }
                _ => {}
            },
            Mode::SendConfirm(item, prompt) => match key.code {
                KeyCode::Esc | KeyCode::Char('n') => {
                    self.mode = Mode::Desk;
                    self.status = "外部送信せず、タスクに残しました".into();
                }
                KeyCode::Char('y') if self.can_confirm => {
                    let (id, revision, prompt) = (item.id, item.revision, prompt.clone());
                    self.launch(id, revision, prompt);
                }
                _ => {}
            },
            Mode::Plan(item, draft) => match key.code {
                KeyCode::Esc => self.mode = Mode::Desk,
                KeyCode::Enter if !draft.trim().is_empty() => {
                    let (field, value) = draft.trim().split_once('=').unwrap_or((draft.trim(), ""));
                    let result = planning::PlanField::parse(field.trim()).and_then(|f| {
                        self.store
                            .change_meta(item.id, item.revision, f, value.trim())
                    });
                    if result.is_ok() {
                        self.mode = Mode::Desk;
                    }
                    self.result(result);
                }
                KeyCode::Backspace => {
                    draft.pop();
                }
                KeyCode::Char(c) if !c.is_control() && draft.len() + c.len_utf8() <= 8000 => {
                    draft.push(c)
                }
                _ => {}
            },
            Mode::Steps(id, row) => {
                let (id, row) = (*id, *row);
                if matches!(key.code, KeyCode::Esc | KeyCode::Char('q')) {
                    self.mode = Mode::Desk;
                    return false;
                }
                let Ok(project) = self.store.show(id) else {
                    self.mode = Mode::Desk;
                    return false;
                };
                let Ok(steps) = self.store.steps(id) else {
                    self.status = "ステップを読み込めません".into();
                    return false;
                };
                let count = steps.len();
                let row = row.min(count.saturating_sub(1));
                match key.code {
                    KeyCode::Char('j') | KeyCode::Down if count > 0 => {
                        self.mode = Mode::Steps(id, (row + 1) % count)
                    }
                    KeyCode::Char('k') | KeyCode::Up if count > 0 => {
                        self.mode = Mode::Steps(id, (row + count - 1) % count)
                    }
                    KeyCode::Char('c') => {
                        self.mode = Mode::StepInput(id, row, false, String::new())
                    }
                    KeyCode::Char('a') => self.mode = Mode::StepInput(id, row, true, String::new()),
                    KeyCode::Char('d' | 'x' | 'J' | 'K') if count > 0 => {
                        let mut after = row;
                        let s = &steps[row];
                        let result = match key.code {
                            KeyCode::Char('d') => {
                                self.store.detach(id, project.revision, s.id, s.revision)
                            }
                            KeyCode::Char('x') => self.store.done(s.id, s.revision, "user"),
                            KeyCode::Char('J' | 'K') => {
                                after = if key.code == KeyCode::Char('J') {
                                    (row + 1).min(count - 1)
                                } else {
                                    row.saturating_sub(1)
                                };
                                let mut order: Vec<_> = steps.iter().map(|s| s.id).collect();
                                order.swap(row, after);
                                self.store.reorder(id, project.revision, &order)
                            }
                            _ => unreachable!(),
                        };
                        self.result(result);
                        self.mode = Mode::Steps(id, after);
                    }
                    _ => {}
                }
            }
            Mode::StepInput(id, row, existing, draft) => match key.code {
                KeyCode::Esc => self.mode = Mode::Steps(*id, *row),
                KeyCode::Enter if !draft.trim().is_empty() => {
                    let (id, row) = (*id, *row);
                    let result = self.store.show(id).and_then(|project| {
                        if *existing {
                            let target = draft
                                .trim()
                                .parse()
                                .map_err(|_| StoreError::Invalid("enter a numeric task ID"))?;
                            let item = self.store.show(target)?;
                            self.store
                                .attach(id, project.revision, item.id, item.revision)
                        } else {
                            let (title, body) = draft.split_once('\n').unwrap_or((draft, ""));
                            self.store.add_step(id, project.revision, title, body)
                        }
                    });
                    let ok = result.is_ok();
                    self.result(result);
                    if ok {
                        self.mode = Mode::Steps(id, row);
                    }
                }
                KeyCode::Backspace => {
                    draft.pop();
                }
                KeyCode::Char(c) if !c.is_control() && draft.len() + c.len_utf8() <= 16000 => {
                    draft.push(c)
                }
                _ => {}
            },
            Mode::Question(id) => {
                let Some(index) = self.questions.iter().position(|q| q.id == *id) else {
                    self.mode = Mode::Desk;
                    self.status = "問いは期限切れか回答済みです".into();
                    return false;
                };
                let q = &self.questions[index];
                match key.code {
                    KeyCode::Esc | KeyCode::Char('q') => self.mode = Mode::Desk,
                    KeyCode::PageDown => {
                        self.question_scroll = self
                            .question_scroll
                            .saturating_add(14)
                            .min(self.question_end)
                    }
                    KeyCode::PageUp => {
                        self.question_scroll = self.question_scroll.saturating_sub(14)
                    }
                    KeyCode::Char('1')
                        if matches!(
                            q.kind,
                            questions::Kind::Approval | questions::Kind::Permission
                        ) && self.question_scroll < self.question_end =>
                    {
                        self.status = "PageDownで問いと選択肢の全文を確認してください".into()
                    }
                    KeyCode::Char('j') | KeyCode::Down => {
                        self.mode =
                            Mode::Question(self.questions[(index + 1) % self.questions.len()].id)
                    }
                    KeyCode::Char('k') | KeyCode::Up => {
                        self.mode = Mode::Question(
                            self.questions
                                [(index + self.questions.len() - 1) % self.questions.len()]
                            .id,
                        )
                    }
                    KeyCode::Char('t')
                        if matches!(q.kind, questions::Kind::Desk | questions::Kind::Worker) =>
                    {
                        self.mode = Mode::QuestionText(q.id, false, String::new())
                    }
                    KeyCode::Char('3') if q.kind == questions::Kind::Permission => {
                        self.mode = Mode::QuestionText(q.id, true, String::new())
                    }
                    KeyCode::Char(c @ '1'..='9') => {
                        self.answer(q.id, questions::Answer::Choice(c as u8 - b'0'))
                    }
                    _ => {}
                }
            }
            Mode::QuestionText(id, reason, draft) => match key.code {
                KeyCode::Esc => self.mode = Mode::Question(*id),
                KeyCode::Enter if !draft.trim().is_empty() => {
                    let msg = std::mem::take(draft);
                    let id = *id;
                    let answer = if *reason {
                        questions::Answer::Reason(&msg)
                    } else {
                        questions::Answer::Text(&msg)
                    };
                    self.answer(id, answer);
                }
                KeyCode::Backspace => {
                    draft.pop();
                }
                KeyCode::Char(c) if !c.is_control() && draft.len() + c.len_utf8() <= 4000 => {
                    draft.push(c)
                }
                _ => {}
            },
            Mode::Compose(item, draft) => match key.code {
                KeyCode::Esc => self.mode = Mode::Desk,
                KeyCode::Enter if !draft.trim().is_empty() => {
                    let id = item.id;
                    let result = self.store.send_message(id, item.revision, draft);
                    if result.is_ok() {
                        self.mode = Mode::Desk;
                        self.result(result);
                        self.detail_scroll = u16::MAX;
                        if self.demo {
                            self.mock_due
                                .insert(id, Instant::now() + Duration::from_millis(900));
                            self.status = "デモ: エージェントが応答を準備中".into();
                        } else if self.agent_config.is_some() {
                            self.confirm_send(id);
                        } else {
                            self.status =
                                "端末に保存済み（未送信）。agent-preview / agent-run で実行".into();
                        }
                    } else if matches!(&result, Err(StoreError::Stale { .. })) {
                        if let Ok(latest) = self.store.show(id) {
                            *item = latest;
                        }
                        self.status =
                            "タスクが変更されたため未保存。内容を確認し、Enterでやり直せます。"
                                .into();
                    } else if let Err(err) = result {
                        self.status = err.to_string();
                    }
                }
                KeyCode::Backspace => {
                    draft.pop();
                }
                KeyCode::Char(c) if !c.is_control() && draft.len() + c.len_utf8() <= 16000 => {
                    draft.push(c)
                }
                _ => {}
            },
            Mode::Filter(draft) => match key.code {
                KeyCode::Esc => self.mode = Mode::Desk,
                KeyCode::Enter => {
                    self.query = draft.trim().to_owned();
                    self.mode = Mode::Desk;
                    self.detail_scroll = 0;
                    self.selected_row = 0;
                    if let Err(e) = self.reload() {
                        self.status = e.to_string();
                    }
                }
                KeyCode::Backspace => {
                    draft.pop();
                }
                KeyCode::Char(c) if !c.is_control() && draft.len() + c.len_utf8() <= 500 => {
                    draft.push(c)
                }
                _ => {}
            },
            Mode::Move(item) => match key.code {
                KeyCode::Esc | KeyCode::Char('q') => self.mode = Mode::Desk,
                KeyCode::Char('9') => {
                    let result = self.store.calendar_action(item.id, item.revision);
                    self.mode = Mode::Desk;
                    self.result(result);
                }
                KeyCode::Char(c @ '1'..='6') => {
                    let result = self.store.move_to(
                        item.id,
                        item.revision,
                        DISPLAY_BOX_IDS[(c as u8 - b'1') as usize],
                        "user",
                    );
                    self.mode = Mode::Desk;
                    self.result(result);
                }
                _ => {}
            },
            Mode::Desk => match key.code {
                KeyCode::Char('q') if self.outgoing.is_none() => return true,
                KeyCode::Char('q') => {
                    self.status = "エージェント実行が終わってから終了できます".into()
                }
                KeyCode::Char('?') => self.mode = Mode::Help,
                KeyCode::Char('c') => self.mode = Mode::Add(String::new()),
                KeyCode::Enter => {
                    if let Some(item) = self.picked() {
                        if self.selected_box == NEXT
                            && self
                                .live_review
                                .as_ref()
                                .is_some_and(|notice| notice.item_id == item.id)
                        {
                            self.note_visible = false;
                            self.mode = Mode::LiveReview;
                        } else {
                            self.mode = Mode::Compose(item.clone(), String::new());
                        }
                    }
                }
                KeyCode::Char('s') if !self.demo && self.agent_config.is_some() => {
                    if let Some(id) = self.picked().map(|x| x.id) {
                        self.confirm_send(id);
                    }
                }
                KeyCode::Char('t') => {
                    if let Some(item) = self.picked() {
                        self.mode = Mode::Plan(item.clone(), String::new());
                    }
                }
                KeyCode::Char('p') => {
                    if let Some(item) = self.picked().filter(|i| i.box_id == "projects") {
                        self.mode = Mode::Steps(item.id, 0);
                    }
                }
                KeyCode::Char('!') => {
                    if self.live_review.is_some() {
                        self.note_visible = false;
                        self.mode = Mode::LiveReview;
                    } else if let Some(q) = self.questions.first() {
                        self.question_scroll = 0;
                        self.question_end = u16::MAX;
                        self.mode = Mode::Question(q.id);
                    } else {
                        self.status = "回答待ちの問いはありません".into();
                    }
                }
                KeyCode::Char('/') => self.mode = Mode::Filter(self.query.clone()),
                KeyCode::Esc if !self.query.is_empty() => {
                    self.query.clear();
                    if let Err(e) = self.reload() {
                        self.status = e.to_string();
                    }
                }
                KeyCode::Char('E') => {
                    if let Some(item) = self.picked() {
                        self.edit_request = Some(item.clone());
                    }
                }
                KeyCode::Char('w') if self.demo => {
                    if let Some(item) = self.picked() {
                        let result = self.store.watch_resolve(
                            item.id,
                            item.revision,
                            &[
                                "https://example.invalid/reviews/1",
                                "https://example.invalid/reviews/2",
                            ],
                        );
                        self.result(result);
                    }
                }
                KeyCode::Char('o') => {
                    if let Some(item) = self.picked() {
                        if self.demo {
                            self.mode = Mode::Artifacts;
                        } else if let Some(path) = &self.profile_path {
                            self.status = match local_files::artifact_dir(path, item.id)
                                .and_then(|dir| local_files::open_artifact_dir(&dir))
                            {
                                Ok(()) => "成果物フォルダを開きました".into(),
                                Err(e) => format!("成果物: {e}"),
                            };
                        }
                    }
                }
                KeyCode::Char('e') => {
                    if let Some(item) = self.picked() {
                        self.mode = Mode::EditBody(item.clone(), item.body.clone());
                    }
                }
                KeyCode::PageDown => {
                    self.detail_scroll = self
                        .detail_scroll
                        .saturating_add(self.detail_page)
                        .min(self.detail_max)
                }
                KeyCode::PageUp => {
                    self.detail_scroll = self.detail_scroll.saturating_sub(self.detail_page)
                }
                KeyCode::Char('f') => {
                    if let Some(item) = self.picked() {
                        self.mode = Mode::Move(item.clone());
                    }
                }
                KeyCode::Char('a') => {
                    if let Some(item) = self.picked() {
                        let who = if item.assignee == "agent" {
                            "me"
                        } else {
                            "agent"
                        };
                        let result = self.store.assign(item.id, item.revision, who);
                        self.result(result);
                    }
                }
                KeyCode::Char('i') => {
                    if let Some(item) = self.picked() {
                        self.mode = Mode::Instruct(item.clone(), item.instructions.clone());
                    }
                }
                KeyCode::Char('s') if self.demo => {
                    if let Some(item) = self.picked().cloned() {
                        match agent::simulate(&mut *self.store, &item) {
                            Ok(()) => {
                                let result = self.store.show(item.id);
                                self.result(result);
                            }
                            Err(e) => self.status = e.to_string(),
                        }
                    }
                }
                KeyCode::Char('x') => {
                    if let Some(item) = self.picked() {
                        let result = self.store.done(item.id, item.revision, "user");
                        self.result(result);
                    }
                }
                KeyCode::Char('r') => {
                    self.status = match self.reload() {
                        Ok(()) => "Refreshed".into(),
                        Err(e) => e.to_string(),
                    };
                }
                KeyCode::Char('l') | KeyCode::Right | KeyCode::Tab => {
                    self.select(self.selected_box + 1)
                }
                KeyCode::Char('h') | KeyCode::Left | KeyCode::BackTab => {
                    self.select(self.selected_box + BOXES.len() - 1)
                }
                KeyCode::Char('j') | KeyCode::Down => {
                    let len = self.entries[self.selected_box].len();
                    if len > 0 {
                        self.selected_row = (self.selected_row + 1) % len;
                    }
                    self.detail_scroll = 0;
                }
                KeyCode::Char('k') | KeyCode::Up => {
                    let len = self.entries[self.selected_box].len();
                    if len > 0 {
                        self.selected_row = (self.selected_row + len - 1) % len;
                    }
                    self.detail_scroll = 0;
                }
                KeyCode::Char(c @ '1'..='6') => self.select((c as u8 - b'1') as usize),
                _ => {}
            },
        }
        false
    }

    fn paste(&mut self, input: &str) {
        let (draft, limit, multiline) = match &mut self.mode {
            Mode::Add(d) => (d, 32_500, true),
            Mode::EditBody(_, d) => (d, 32_000, true),
            Mode::Instruct(_, d) | Mode::Compose(_, d) => (d, 16_000, true),
            Mode::Filter(d) => (d, 500, false),
            Mode::Plan(_, d) => (d, 8000, false),
            Mode::StepInput(_, _, existing, d) => (d, 16000, !*existing),
            Mode::QuestionText(_, _, d) => (d, 4000, true),
            _ => return,
        };
        for c in input.replace("\r\n", "\n").replace('\r', "\n").chars() {
            if c.is_control() && !(multiline && matches!(c, '\n' | '\t')) {
                continue;
            }
            if draft.len() + c.len_utf8() > limit {
                break;
            }
            draft.push(c);
        }
    }

    fn confirm_send(&mut self, id: i64) {
        if self.outgoing.is_some() {
            self.status = "別のエージェント実行が進行中です。送信はしていません".into();
            return;
        }
        match self.store.show(id).and_then(|item| {
            if !item.pending_message && (item.assignee != "agent" || item.instructions.is_empty()) {
                return Err(StoreError::Invalid("送る指示がありません"));
            }
            Ok((item, self.store.agent_prompt(id)?))
        }) {
            Ok((item, prompt)) => {
                self.can_confirm = false;
                self.mode = Mode::SendConfirm(item, prompt);
            }
            Err(e) => self.status = e.to_string(),
        }
    }

    fn launch(&mut self, id: i64, rev: i64, prompt: String) {
        let (Some(path), Some(config)) = (self.profile_path.clone(), self.agent_config.clone())
        else {
            return;
        };
        let (tx, rx) = mpsc::channel();
        self.outgoing = Some(rx);
        let settings = self.review_settings.clone();
        let (review_tx, review_rx) = mpsc::sync_channel(1);
        self.review_rx = settings.as_ref().map(|_| review_rx);
        self.mode = Mode::Desk;
        self.status = "エージェントを起動しています".into();
        std::thread::spawn(move || {
            let result = store::Sqlite::open(&path)
                .map_err(|e| e.to_string())
                .and_then(|mut store| {
                    match settings {
                        Some(settings) => review_agent::run(
                            &mut store, id, rev, &config, &prompt, &settings, review_tx,
                        ),
                        None => agent::run_verified(&mut store, id, rev, &config, Some(&prompt)),
                    }
                    .map_err(|e| e.to_string())
                });
            let status = match result {
                Ok(_) => "エージェントの返事が届きました".into(),
                Err(e) => format!("エージェント実行: {e}"),
            };
            let _ = tx.send(status);
        });
    }

    fn answer(&mut self, id: i64, answer: questions::Answer<'_>) {
        self.status = match self.store.answer(id, answer, questions::now()) {
            Ok(()) => "回答をローカルに記録しました（外部操作なし）".into(),
            Err(e) => e.to_string(),
        };
        self.mode = Mode::Desk;
        if let Ok(q) = self.store.questions(questions::now()) {
            self.questions = q;
        }
    }

    fn tick(&mut self, now: Instant) {
        if self.live_review.as_ref().is_some_and(|n| now >= n.deadline) {
            self.live_review = None;
            let _ = self.reload();
            self.note_visible = false;
            if self.mode == Mode::LiveReview {
                self.mode = Mode::Desk;
            }
        }
        if self.live_review.is_none()
            && let Some(rx) = &self.review_rx
            && let Ok(notice) = rx.try_recv()
            && now < notice.deadline
        {
            self.live_review = Some(notice);
            let _ = self.reload();
        }
        if let Some(watcher) = &mut self.pr_watcher {
            match watcher.tick(&mut *self.store, now) {
                Ok(n) if n > 0 => {
                    let _ = self.reload();
                    self.status = format!("すべてのPRが閉じた {n} 件を次の行動へ移しました");
                }
                Err(e) => self.status = e.to_string(),
                _ => {}
            }
        }
        if let Some(rx) = &self.outgoing {
            match rx.try_recv() {
                Ok(message) => {
                    self.outgoing = None;
                    self.review_rx = None;
                    self.live_review = None;
                    self.note_visible = false;
                    if self.mode == Mode::LiveReview {
                        self.mode = Mode::Desk;
                    }
                    self.status = message;
                    let _ = self.reload();
                }
                Err(mpsc::TryRecvError::Disconnected) => {
                    self.outgoing = None;
                    self.review_rx = None;
                    self.live_review = None;
                    self.note_visible = false;
                    if self.mode == Mode::LiveReview {
                        self.mode = Mode::Desk;
                    }
                    self.status = "実行プロセスが終了しました".into();
                }
                Err(mpsc::TryRecvError::Empty) => {}
            }
        }
        if let Ok(q) = self.store.questions(questions::now()) {
            self.questions = q;
        }
        let ready: Vec<_> = self
            .mock_due
            .iter()
            .filter_map(|(&id, &when)| (now >= when).then_some(id))
            .collect();
        for id in ready {
            self.mock_due.remove(&id);
            let selected =
                self.picked().map(|x| x.id) == Some(id) && matches!(self.mode, Mode::Desk);
            let result = self
                .store
                .show(id)
                .map_err(|e| e.to_string())
                .and_then(|item| {
                    agent::simulate(&mut *self.store, &item).map_err(|e| e.to_string())
                });
            match result {
                Ok(()) if selected => {
                    let result = self.store.show(id);
                    self.result(result);
                    self.detail_scroll = u16::MAX;
                    self.status = "デモ: 返事が届きました".into();
                }
                Ok(()) => {
                    let _ = self.reload();
                }
                Err(e) => self.status = e,
            }
        }
    }

    fn observe_read(&mut self, now: Instant) {
        let Some(item) = self
            .picked()
            .filter(|item| item.unread_reply && matches!(self.mode, Mode::Desk))
        else {
            self.reading = None;
            return;
        };
        let id = item.id;
        let Ok(messages) = self.store.conversation(id) else {
            return;
        };
        let Some(through) = messages
            .iter()
            .rev()
            .find(|m| m.role == "agent" && !m.read)
            .map(|m| m.id)
        else {
            return;
        };
        if let Some((old_id, old_reply, since)) = self.reading
            && (old_id, old_reply) == (id, through)
        {
            if now.saturating_duration_since(since) >= Duration::from_millis(800)
                && self.store.mark_read(id, through).is_ok()
            {
                self.reading = None;
                let _ = self.reload();
            }
        } else {
            self.reading = Some((id, through, now));
        }
    }

    fn draw(&mut self, f: &mut Frame) {
        let area = f.area();
        if area.width < MIN_WIDTH || area.height < MIN_HEIGHT {
            f.render_widget(
                Paragraph::new(
                    "lazygtd needs 66 columns and 27 rows. Resize to continue; Ctrl+C quits.",
                )
                .wrap(Wrap { trim: true }),
                area,
            );
            return;
        }
        f.render_widget(Block::new().style(BASE), area);
        let [board, footer] =
            Layout::vertical([Constraint::Min(0), Constraint::Length(2)]).areas(area);
        let [sidebar, right] =
            Layout::horizontal([Constraint::Percentage(42), Constraint::Fill(1)]).areas(board);
        let [detail, compose] =
            Layout::vertical([Constraint::Min(0), Constraint::Length(6)]).areas(right);
        for (i, rect) in sidebar_slots(sidebar, self.selected_box)
            .into_iter()
            .enumerate()
        {
            let selected = i == self.selected_box;
            let total = self.entries[i].len();
            let position = if selected && total > 0 {
                self.selected_row.min(total - 1) + 1
            } else {
                0
            };
            let block = Block::default()
                .borders(Borders::ALL)
                .style(BASE)
                .border_style(if selected { FOCUS } else { BASE })
                .title(format!(" {} {} ", i + 1, BOXES[i]))
                .title_bottom(Line::from(format!(" {position} of {total} ")).right_aligned());
            if sidebar_collapsed(i, self.selected_box) {
                f.render_widget(block, rect);
                continue;
            }
            let capacity = rect.height.saturating_sub(2) as usize;
            let question_count = self.questions.len() + usize::from(self.live_review.is_some());
            let show_question_row =
                i == NEXT && question_count > 0 && capacity > usize::from(selected);
            let capacity = capacity.saturating_sub(usize::from(show_question_row));
            let offset = if selected {
                self.selected_row.saturating_sub(capacity.saturating_sub(1))
            } else {
                0
            };
            let task_items = self.entries[i]
                .iter()
                .enumerate()
                .skip(offset)
                .map(|(row, item)| {
                    ListItem::new(format!(
                        "{}{}{}",
                        if i == NEXT
                            && let Some(review) =
                                self.live_review.as_ref().filter(|n| n.item_id == item.id)
                        {
                            review.badge()
                        } else if item.unread_reply {
                            "● "
                        } else if item.pending_message {
                            "… "
                        } else {
                            ""
                        },
                        if item.assignee == "agent" {
                            "AI · "
                        } else if i == NEXT {
                            "自分 · "
                        } else {
                            ""
                        },
                        format_task(item, &self.today)
                    ))
                    .style(if selected && row == self.selected_row {
                        SELECTED
                    } else {
                        BASE
                    })
                });
            let questions = if show_question_row {
                vec![
                    ListItem::new(format!("! 問い ({question_count})"))
                        .style(Style::new().fg(Color::Rgb(224, 175, 104))),
                ]
            } else {
                Vec::new()
            };
            let items = questions.into_iter().chain(task_items).collect::<Vec<_>>();
            f.render_widget(List::new(items).block(block), rect);
        }
        let title = self.picked().map_or_else(
            || "(empty box)".into(),
            |x| {
                let who = if x.assignee == "agent" {
                    "エージェント"
                } else {
                    "自分"
                };
                let mut summary =
                    format!("#{}  r{}  担当: {}\n\n{}", x.id, x.revision, who, x.title);
                let m = &x.planning;
                for (label, value) in [
                    ("締切", &m.due),
                    ("確認日", &m.follow_up),
                    ("待ち相手", &m.who),
                    ("次にやること", &m.then),
                    ("監視URL（参照のみ）", &m.watch),
                    ("成果", &m.outcome),
                    ("略称", &m.short),
                    ("状態の変更日", &m.since),
                ] {
                    if let Some(v) = value {
                        summary.push_str(&format!("\n{label}: {v}"));
                    }
                }
                for link in &m.links {
                    summary.push_str(&format!("\n資料: {link}"));
                }
                if let Some(id) = x.project_id {
                    summary.push_str(&format!("\nプロジェクト: #{id}"));
                }
                if x.box_id == "projects"
                    && let Ok(steps) = self.store.steps(x.id)
                    && !steps.is_empty()
                {
                    summary.push_str("\n\n## ステップ（p で操作）");
                    for step in steps {
                        summary.push_str(&format!(
                            "\n- [{}] {} (#{} )",
                            step.state, step.title, step.id
                        ));
                    }
                }
                if !x.body.is_empty() {
                    summary.push_str(&format!("\n\n{}", x.body));
                }
                if !x.instructions.is_empty() {
                    summary.push_str(&format!("\n\n指示: {}", x.instructions));
                }
                match self.store.latest_run(x.id) {
                    Ok(Some(run)) => {
                        summary.push_str(&format!(
                            "\n\n実行 #{} · {} · {} ({})",
                            run.id, run.provider, run.state, run.activity
                        ));
                        if let Some(native) = &run.session_id {
                            summary.push_str(&format!("\nセッション: {native}"));
                        }
                        summary.push_str(&format!("\n{}", run.result));
                    }
                    Err(e) => summary.push_str(&format!("\n\n実行状況の取得に失敗: {e}")),
                    Ok(None) => {}
                }
                if let Ok(messages) = self.store.conversation(x.id)
                    && !messages.is_empty()
                {
                    summary.push_str("\n\n## エージェントとの会話");
                    for m in messages {
                        let name = if m.role == "agent" {
                            "エージェント"
                        } else if m.run_id.is_none() {
                            "自分（未送信）"
                        } else {
                            "自分"
                        };
                        summary.push_str(&format!("\n{name}: {}\n", m.body));
                    }
                }
                summary
            },
        );
        let profile = if self.demo {
            "DEMO (disposable)"
        } else {
            "WORK (saved locally)"
        };
        let question_status = if self.live_review.is_some() {
            self.live_review
                .as_ref()
                .map_or(String::new(), |n| format!(" ! {}の承認待ち", n.provider))
        } else if self.questions.is_empty() {
            String::new()
        } else {
            format!(" ! 問い {}", self.questions.len())
        };
        let width = usize::from(detail.width.saturating_sub(2).max(1));
        let styles = render::markdown(&title, detail.width.saturating_sub(2));
        let paragraph = Paragraph::new(Text::from(styles))
            .style(BASE)
            .block(Block::bordered().title(" Details "))
            .wrap(Wrap { trim: false });
        self.detail_page = (detail.height.saturating_sub(2) / 2).max(1);
        self.detail_max = paragraph
            .line_count(width as u16)
            .saturating_sub(usize::from(detail.height))
            .min(u16::MAX as usize) as u16;
        self.detail_scroll = self.detail_scroll.min(self.detail_max);
        f.render_widget(paragraph.scroll((self.detail_scroll, 0)), detail);
        let (reply, active) = match &self.mode {
            Mode::Compose(_, draft) => (
                draft
                    .rsplitn(3, '\n')
                    .collect::<Vec<_>>()
                    .into_iter()
                    .rev()
                    .collect::<Vec<_>>()
                    .join("\n"),
                true,
            ),
            _ if self.picked().is_some() => ("Enter: 指示・返事を書く".into(), false),
            _ => ("タスクを選ぶと入力できます".into(), false),
        };
        let caption = if self.demo {
            " エージェントへ（デモ） "
        } else {
            " エージェントへ（ローカルキュー） "
        };
        f.render_widget(
            Paragraph::new(reply)
                .style(BASE)
                .block(
                    Block::bordered()
                        .border_style(if active { FOCUS } else { BASE })
                        .title(caption),
                )
                .wrap(Wrap { trim: false }),
            compose,
        );
        let line = match &self.mode {
            Mode::Compose(_, _) => "Enter: 確定   Ctrl+J / Shift+Enter: 改行   Esc: 取消".into(),
            Mode::Filter(query) => format!("/{query}   Enter: apply   Esc: cancel"),
            _ if !self.query.is_empty() => {
                format!("Filter: {}   / edit   Esc clear   ? help", self.query)
            }
            _ => "1-6 box  j/k rows  c add  e body  / filter  ? help  q quit".into(),
        };
        f.render_widget(
            Paragraph::new(format!(
                "{profile}{question_status}  {}\n{line}",
                self.status
            ))
            .style(Style::new().fg(BLUE).bg(BG)),
            footer,
        );
        match &self.mode {
            Mode::Desk | Mode::Filter(_) | Mode::Compose(..) => {}
            Mode::Help => popup(f, area, "Keys", "1-6 or h/l/Tab: box  j/k: row\nc: add  f: move  x: done  r: reload\na: assign  i: instructions  s: simulate (demo only)\ne: body  /: filter  Ctrl+D/U: scroll\np: steps  t: settings  !: questions  Enter: reply\nE: external editor  o: artifacts  w: check demo watch\n?: help   q: close this panel or quit".into(), 10),
            Mode::Add(draft) => popup(f, area, "Add Inbox item", editor_text(draft,"Enter: add"), 11),
            Mode::Instruct(_,draft) => popup(f,area,"Agent instructions",editor_text(draft,"Enter: save locally"),11),
            Mode::EditBody(_,draft)=>popup(f,area,"Edit body",editor_text(draft,"Enter: save"),11),
            Mode::Plan(_,draft)=>popup(f,area,"項目の情報",format!("due=YYYY-MM-DD / 空で解除\n連絡待ち: follow_up= / who= / then= / watch=\nプロジェクト: outcome= / short=　共通: links=\n\n{draft}\n\nEnter: 保存　Esc: 取消"),10),
            Mode::Steps(id,row)=>{
                let steps=self.store.steps(*id).unwrap_or_default();
                let from=row.saturating_sub(6).min(steps.len().saturating_sub(1));
                let lines=steps.iter().enumerate().skip(from).take(9).map(|(i,s)|format!("{} [{}] #{} {}",if i==*row{">"}else{" "},s.state,s.id,s.title)).collect::<Vec<_>>().join("\n");
                popup(f,area,"プロジェクトのステップ",format!("{lines}\n\nc: 作る　a: 既存IDを追加　d: 外す　x: 完了\nJ/K: 順序　j/k: 選択　Esc: 戻る"),16)
            }
            Mode::StepInput(_,_,existing,draft)=>popup(f,area,if *existing {"追加するタスクID"}else{"ステップを作る"},editor_text(draft,"Enter: 追加"),11),
            Mode::Question(id)=>{
                if let Some(q)=self.questions.iter().find(|q|q.id==*id){
                    let deadline=q.deadline.map(|d|format!("　残り{}秒",d.saturating_sub(questions::now()))).unwrap_or_default();
                    let opts=q.choices.iter().map(|o|format!("{}: {}{}",o.id,o.label,if o.recommended{"（推奨）"}else{""})).collect::<Vec<_>>().join("\n");
                    let text=if matches!(q.kind,questions::Kind::Desk|questions::Kind::Worker){"　t: 自由記入"}else{""};
                    let content=format!("{}{deadline}\n\n{}\n\n{opts}\n\nj/k: 別の問い{text}　PgUp/PgDn: 読む　Esc: 戻る",q.title,q.body);
                    let width=area.width.min(70);let height=21.min(area.height-2);
                    let rect=Rect::new(area.x+(area.width-width)/2,area.y+(area.height-height)/2,width,height);
                    let block=Block::bordered().title(q.kind.label()).border_style(Style::new().fg(Color::Rgb(224,175,104)));
                    let paragraph=Paragraph::new(render::markdown(&content,width-2)).style(BASE)
                        .block(block).wrap(Wrap{trim:false});
                    self.question_end=paragraph.line_count(width-2).saturating_sub(height as usize).min(u16::MAX as usize) as u16;
                    self.question_scroll=self.question_scroll.min(self.question_end);
                    f.render_widget(Clear,rect);
                    f.render_widget(paragraph.scroll((self.question_scroll,0)),rect);
                }
            }
            Mode::Artifacts=>popup(f,area,"成果物（デモ）","report.md\n\n## 試作の確認結果\nタイトルと本文からタスクを作成できました。\n次は、実行結果を「次の行動」で確認してください。\n\nEsc: 閉じる".into(),10),
            Mode::LiveReview => {
                self.note_visible = false;
                if let Some(notice) = &self.live_review {
                    let remaining = notice.deadline.saturating_duration_since(Instant::now()).as_secs();
                    let base = format!("{} / 残り {remaining} 秒
", notice.provider);
                    let height = area.height.saturating_sub(4).min(32);
                    let width = area.width.min(64).saturating_sub(2);
                    let note = self.review_settings.as_ref().and_then(|c| c.note.as_deref());
                    let detail = notice.detail.as_deref().unwrap_or("");
                    let mut body = format!("{base}\n要求: {}\n{detail}\nこの画面では実行権限は与えません。\nn / Esc: 拒否　c: 実行を中断", notice.method);
                    if notice.method == "item/tool/call" && let Some(note) = note {
                        let destination = self.agent_config.as_ref().map_or_else(String::new, agent::Config::destination);
                        let candidate = format!("{base}送信先: {destination}
{}

{}

y: この全文を一度だけ送信
n / Esc: 拒否　c: 実行を中断", note.path().display(), note.content());
                        self.note_visible = Paragraph::new(candidate.as_str()).wrap(Wrap { trim:false })
                            .line_count(width) <= usize::from(height.saturating_sub(2));
                        body = if self.note_visible { candidate } else {
                            format!("{base}
メモ全文が見えないため許可できません。
端末を広げるか拒否してください。
n / Esc: 拒否　c: 実行を中断")
                        };
                    }
                    popup(f,area,"エージェントの承認待ち",body,height);
                }
            }
            Mode::SendConfirm(_,prompt)=>{
                let destination=self.agent_config.as_ref().map_or_else(String::new,agent::Config::destination);
                let scope=if self.review_settings.as_ref().is_some_and(|s| s.note.is_some()) {
                    "ファイル読み取り: 無効を要求。指定メモは別の確認が必要"
                } else if self.agent_config.as_ref().is_some_and(agent::Config::allow_file_reads) {
                    "CLIのファイル読み取り: 有効"
                } else {
                    "CLIのファイル読み取り: 無効を要求"
                };
                let history = self.agent_config.as_ref().and_then(agent::Config::history_warning).unwrap_or("");
                let review = self.review_settings.as_ref().map_or_else(String::new, |s| format!("\n{}（対話確認つき）", s.provider.label()));
                let preview=format!("送信先: {destination}\n{scope}{review}\n{}{}\n\n{prompt}",agent::TRUST_WARNING, if history.is_empty() { String::new() } else { format!("\n{history}") });
                let w=area.width.min(64).saturating_sub(2);
                let height=area.height.saturating_sub(4).min(28);
                let paragraph=Paragraph::new(preview.as_str()).wrap(Wrap{trim:false});
                self.can_confirm=paragraph.line_count(w)+4 <= usize::from(height);
                let message=if self.can_confirm {
                    format!("{preview}\n\ny: この内容で送信　Esc: 未送信のまま戻る")
                } else {
                    format!("送信先: {destination}\n\n全文をこの画面に表示できないため送信しません。\nagent-preview で全文を確認し、agent-run を使ってください。\nEsc: 戻る")
                };
                popup(f,area,"外部送信の確認",message,height);
            }
            Mode::QuestionText(_,_,draft)=>popup(f,area,"問いへの回答",editor_text(draft,"Enter: 記録"),11),
            Mode::Move(item) => popup(f, area, "Move item", format!("#{}: {}\n1 Inbox  2 次の行動  3 プロジェクト\n4 連絡待ち  5 いつか  6 資料\n9: カレンダーに入れる（次の行動）\nEsc: cancel", item.id, item.title), 8),
        }
    }
}

fn format_task(item: &Item, today: &str) -> String {
    let mut reasons = planning::review_reasons(&item.planning, &item.box_id, today);
    if item.agent_review {
        reasons.push("確認");
    }
    if item.planning.calendar_action {
        reasons.push("カレンダーに入れる");
    }
    if reasons.is_empty() {
        item.title.clone()
    } else {
        format!("{} · {}", reasons.join("/"), item.title)
    }
}

fn editor_text(draft: &str, action: &str) -> String {
    let preview = draft
        .rsplitn(6, '\n')
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect::<Vec<_>>()
        .join("\n");
    format!("{preview}\n\n{action}  Ctrl+J / Shift+Enter: newline  Esc: cancel")
}

fn sidebar_collapsed(index: usize, selected: usize) -> bool {
    matches!(DISPLAY_BOX_IDS[index], "someday" | "reference") && index != selected
}

fn sidebar_slots(sidebar: Rect, selected: usize) -> [Rect; BOXES.len()] {
    let collapsed = (0..BOXES.len())
        .filter(|&i| sidebar_collapsed(i, selected))
        .count() as u16;
    let header_height = 2.min(sidebar.height / BOXES.len() as u16);
    let open = BOXES.len() as u16 - collapsed;
    let height_for_open = sidebar.height - collapsed * header_height;
    let mut y = sidebar.y;
    let mut slot = 0;
    std::array::from_fn(|i| {
        let height = if sidebar_collapsed(i, selected) {
            header_height
        } else {
            let extra = u16::from(slot < height_for_open % open);
            slot += 1;
            height_for_open / open + extra
        };
        let rect = Rect::new(sidebar.x, y, sidebar.width, height);
        y += height;
        rect
    })
}

fn popup(f: &mut Frame, area: Rect, title: &str, body: String, height: u16) {
    let width = area.width.min(64);
    let rect = Rect::new(
        area.x + (area.width - width) / 2,
        area.y + (area.height - height) / 2,
        width,
        height,
    );
    f.render_widget(Clear, rect);
    f.render_widget(
        Paragraph::new(body)
            .block(Block::bordered().title(title))
            .wrap(Wrap { trim: false }),
        rect,
    );
}

fn tui(
    store: Box<dyn Store>,
    demo: bool,
    path: Option<PathBuf>,
    config: Option<agent::Config>,
    pr_watcher: Option<pr_watch::Watcher>,
    review_settings: Option<review_agent::Settings>,
) -> io::Result<()> {
    let mut desk = Desk::new(store, demo).map_err(io::Error::other)?;
    desk.profile_path = path;
    desk.agent_config = config;
    desk.pr_watcher = pr_watcher;
    desk.review_settings = review_settings;
    let mut terminal = ratatui::try_init()?;
    let bracketed = execute!(io::stdout(), EnableBracketedPaste).is_ok();
    let result = (|| {
        loop {
            desk.tick(Instant::now());
            terminal.draw(|f| desk.draw(f))?;
            if terminal
                .size()
                .is_ok_and(|s| s.width >= MIN_WIDTH && s.height >= MIN_HEIGHT)
            {
                desk.observe_read(Instant::now());
            }
            if !event::poll(Duration::from_millis(250))? {
                if !demo
                    && matches!(desk.mode, Mode::Desk)
                    && let Err(e) = desk.reload()
                {
                    desk.status = format!("Refresh failed: {e}");
                }
                continue;
            }
            let input = event::read()?;
            if let Event::Paste(text) = input {
                desk.paste(&text);
                continue;
            }
            if let Event::Key(key) = input {
                let large_enough = terminal
                    .size()
                    .is_ok_and(|s| s.width >= MIN_WIDTH && s.height >= MIN_HEIGHT);
                if !large_enough {
                    desk.can_confirm = false;
                    desk.note_visible = false;
                }
                // Do not accidentally send or edit a hidden draft on a tiny terminal.
                let quit =
                    key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c');
                if (!large_enough && quit && desk.outgoing.is_none())
                    || (large_enough && desk.key(key))
                {
                    return Ok(());
                }
                if let Some(item) = desk.edit_request.take() {
                    if bracketed {
                        let _ = execute!(io::stdout(), DisableBracketedPaste);
                    }
                    ratatui::try_restore()?;
                    let new = local_files::edit_body(&item.body);
                    terminal = ratatui::try_init()?;
                    if bracketed {
                        let _ = execute!(io::stdout(), EnableBracketedPaste);
                    }
                    match new {
                        Ok(body) => {
                            let result = desk.store.edit_body(item.id, item.revision, &body);
                            if matches!(result, Err(StoreError::Stale { .. }))
                                && let Ok(latest) = desk.store.show(item.id)
                            {
                                desk.mode = Mode::EditBody(latest, body);
                                desk.status =
                                    "同時更新があり、外部エディタの編集内容を下書きに保持しました"
                                        .into();
                                continue;
                            }
                            desk.result(result);
                        }
                        Err(e) => desk.status = format!("エディタ: {e}"),
                    }
                }
            }
        }
    })();
    if bracketed {
        let _ = execute!(io::stdout(), DisableBracketedPaste);
    }
    let restored = ratatui::try_restore();
    result.and(restored)
}

fn main() -> ExitCode {
    let args: Vec<_> = env::args_os().skip(1).collect();
    if let Err(e) = cli::run(&args) {
        eprintln!("lazygtd: {e}");
        return ExitCode::from(cli::status(e.as_ref()));
    }
    ExitCode::SUCCESS
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::{Terminal, backend::TestBackend};

    fn press(desk: &mut Desk, c: char) -> bool {
        desk.key(KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE))
    }

    fn view(desk: &mut Desk, width: u16, height: u16) -> Vec<String> {
        let mut term = Terminal::new(TestBackend::new(width, height)).unwrap();
        term.draw(|frame| desk.draw(frame)).unwrap();
        let b = term.backend().buffer();
        (0..height)
            .map(|y| (0..width).map(|x| b[(x, y)].symbol()).collect())
            .collect()
    }

    #[test]
    fn compose_is_per_task_and_simulated_send_builds_a_thread() {
        let mut desk = Desk::demo();
        let enter = KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE);
        press(&mut desk, '2');
        desk.selected_row = desk.entries[NEXT]
            .iter()
            .position(|item| item.box_id == "next" && item.assignee == "agent")
            .unwrap();
        let id = desk.picked().unwrap().id;
        desk.key(enter);
        assert!(matches!(desk.mode, Mode::Compose(..)));
        desk.key(enter);
        assert!(desk.store.conversation(id).unwrap().is_empty());
        desk.paste("次の指示です\n- 確認をお願いします");
        desk.key(enter);
        assert!(desk.store.show(id).unwrap().pending_message);
        assert_eq!(desk.store.conversation(id).unwrap().len(), 1);
        assert!(desk.entries[NEXT].iter().any(|row| row.id == id));
        desk.tick(Instant::now() + Duration::from_secs(2));
        let row = desk.entries[NEXT]
            .iter()
            .position(|row| row.id == id)
            .unwrap();
        desk.select(NEXT);
        desk.selected_row = row;
        let item = desk.picked().unwrap();
        assert_eq!(item.id, id);
        assert_eq!(item.box_id, "review");
        assert!(item.unread_reply);
        let list = desk.store.conversation(id).unwrap();
        assert_eq!(list.len(), 2);
        assert!(list[0].body.contains("- 確認"));
        assert_eq!(list[1].role, "agent");
        assert!(desk.store.conversation(id - 1).unwrap().is_empty());
        let now = Instant::now();
        desk.observe_read(now);
        desk.observe_read(now + Duration::from_millis(790));
        assert!(desk.store.show(id).unwrap().unread_reply);
        desk.observe_read(now + Duration::from_millis(810));
        assert!(!desk.store.show(id).unwrap().unread_reply);
        desk.key(enter);
        desk.paste("さらにお願いします");
        desk.key(enter);
        desk.tick(Instant::now() + Duration::from_secs(2));
        assert_eq!(desk.store.conversation(id).unwrap().len(), 4);
        assert!(!desk.store.show(id).unwrap().done);
    }

    #[test]
    fn multiline_add_and_filter_cannot_submit_on_newline() {
        let mut desk = Desk::demo();
        let enter = |modifiers| KeyEvent::new(KeyCode::Enter, modifiers);
        let ctrl_j = KeyEvent::new(KeyCode::Char('j'), KeyModifiers::CONTROL);
        let previous = desk.store.list().unwrap().len();
        press(&mut desk, 'c');
        for ch in "テスト".chars() {
            press(&mut desk, ch);
        }
        desk.key(ctrl_j);
        for ch in "本文XYZ".chars() {
            press(&mut desk, ch);
        }
        desk.key(enter(KeyModifiers::SHIFT));
        press(&mut desk, 'q');
        assert_eq!(desk.store.list().unwrap().len(), previous);
        desk.key(enter(KeyModifiers::NONE));
        assert_eq!(desk.picked().unwrap().body, "本文XYZ\nq");
        press(&mut desk, '/');
        for ch in "xyz".chars() {
            press(&mut desk, ch);
        }
        desk.key(ctrl_j);
        desk.key(enter(KeyModifiers::SHIFT));
        assert!(matches!(desk.mode, Mode::Filter(_)));
        desk.key(enter(KeyModifiers::NONE));
        assert_eq!(desk.entries[INBOX].len(), 1);
        press(&mut desk, 'l');
        assert_eq!(desk.entries[INBOX].len(), 3);
    }

    #[test]
    fn demo_agent_returns_to_human_review_without_finishing() {
        let mut desk = Desk::demo();
        press(&mut desk, '2');
        desk.selected_row = desk.entries[NEXT]
            .iter()
            .position(|item| item.box_id == "next" && item.assignee == "agent")
            .unwrap();
        let id = desk.picked().unwrap().id;
        assert_eq!(desk.picked().unwrap().assignee, "agent");
        press(&mut desk, 's');
        let reviewed = desk.picked().unwrap();
        assert_eq!(reviewed.id, id);
        assert_eq!(reviewed.box_id, "review");
        assert_eq!(reviewed.assignee, "me");
        assert!(!reviewed.done);
        assert!(
            view(&mut desk, 100, 34)
                .join("\n")
                .contains("demo · review")
        );
    }

    #[test]
    fn expanded_reference_does_not_resize_when_finishing_items() {
        let mut desk = Desk::demo();
        let titles = |frame: &[String]| {
            BOXES.map(|title| {
                frame
                    .iter()
                    .position(|line| {
                        line.chars()
                            .take(38)
                            .filter(|c| !c.is_whitespace())
                            .collect::<String>()
                            .contains(title)
                    })
                    .unwrap()
            })
        };
        let before = view(&mut desk, 100, 31);
        let expected = titles(&before);
        assert_eq!(expected, [0, 7, 13, 19, 25, 27]);
        press(&mut desk, '1');
        assert_eq!(titles(&view(&mut desk, 100, 31)), expected);
        desk.selected_row = desk.entries[INBOX]
            .iter()
            .position(|item| item.title == "明日の会議を準備する")
            .unwrap();
        assert!(view(&mut desk, 100, 31).iter().any(|line| {
            line.replace(char::is_whitespace, "")
                .contains("明日の会議を準備する")
        }));
        press(&mut desk, '6');
        let after = view(&mut desk, 100, 31);
        let expanded = titles(&after);
        assert_eq!(expanded, [0, 6, 12, 17, 22, 24]);
        assert!(after.iter().any(|line| {
            line.replace(char::is_whitespace, "")
                .contains("キーバインドの早見表")
        }));
        press(&mut desk, 'x');
        press(&mut desk, 'x');
        let empty = view(&mut desk, 100, 31);
        assert_eq!(titles(&empty), expanded);
        assert!(empty.iter().any(|line| line.contains("(empty box)")));
    }

    #[test]
    fn low_priority_boxes_collapse_and_counters_track_focus_filter_and_empty_lists() {
        let mut desk = Desk::demo();
        let someday = 4;
        let reference = 5;

        let read = |desk: &mut Desk, i: usize| {
            let frame = view(desk, 100, 40);
            let rect = sidebar_slots(Rect::new(0, 0, 42, 38), desk.selected_box)[i];
            let line = |y| frame[y as usize].chars().take(42).collect::<String>();
            // The counter must be flush right on the bottom border.
            assert!(line(rect.bottom() - 1).ends_with(" ┘"));
            let bottom = line(rect.bottom() - 1);
            (
                rect.height,
                bottom,
                (rect.y + 1..rect.bottom() - 1)
                    .map(line)
                    .collect::<Vec<_>>(),
            )
        };
        for i in [someday, reference] {
            let (height, bottom, rows) = read(&mut desk, i);
            assert_eq!(height, 2);
            assert!(rows.is_empty());
            assert!(bottom.ends_with(&format!(" 0 of {} ┘", desk.entries[i].len())));
        }
        press(&mut desk, '6');
        let (h, footer, rows) = read(&mut desk, reference);
        assert!(h > 2);
        assert!(footer.ends_with(" 1 of 2 ┘"));
        assert!(
            rows.join("\n")
                .replace(char::is_whitespace, "")
                .contains("キーバインドの早見表")
        );
        press(&mut desk, 'j');
        assert!(read(&mut desk, reference).1.ends_with(" 2 of 2 ┘"));
        desk.query = "does-not-match-the-demo".into();
        desk.reload().unwrap();
        assert!(read(&mut desk, reference).1.ends_with(" 0 of 0 ┘"));
        press(&mut desk, '5');
        assert!(read(&mut desk, someday).0 > 2);
        assert_eq!(read(&mut desk, reference).0, 2);
    }

    #[test]
    fn next_actions_keep_legacy_rows_and_overlays_a_claude_approval_only_once() {
        let mut desk = Desk::demo();
        let legacy = desk.entries[NEXT]
            .iter()
            .filter(|it| matches!(it.box_id.as_str(), "review" | "desk"))
            .count();
        assert!(legacy > 0);
        let task = desk.entries[1]
            .iter()
            .find(|it| it.assignee == "agent")
            .unwrap()
            .clone();
        let (notice, rx) = review_agent::Notice::test_claude(task.id);
        desk.live_review = Some(notice);
        desk.reload().unwrap();
        assert_eq!(desk.entries[NEXT][0].id, task.id);
        assert_eq!(desk.entries[NEXT][0].assignee, "me");
        assert!(!desk.entries[INBOX].iter().any(|it| it.id == task.id));
        assert_eq!(
            desk.entries[NEXT]
                .iter()
                .filter(|it| it.id == task.id)
                .count(),
            1
        );
        assert_eq!(desk.store.show(task.id).unwrap().assignee, "agent");
        assert!(view(&mut desk, 100, 36).join("\n").contains("Claude"));
        desk.select(NEXT);
        desk.selected_row = 0;
        desk.key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert_eq!(desk.mode, Mode::LiveReview);
        press(&mut desk, 'n');
        assert_eq!(
            rx.try_recv().unwrap(),
            approval_server::ReviewDecision::Deny
        );
        assert!(desk.live_review.is_none());
        assert_eq!(
            desk.entries[NEXT]
                .iter()
                .find(|it| it.id == task.id)
                .unwrap()
                .assignee,
            "agent"
        );
    }

    #[test]
    fn q_is_text_in_editor_but_closes_help_or_exits_on_desk() {
        let mut desk = Desk::demo();
        assert!(!press(&mut desk, '?'));
        assert_eq!(desk.mode, Mode::Help);
        assert!(!press(&mut desk, 'q'));
        assert!(!press(&mut desk, 'c'));
        for c in "quote".chars() {
            assert!(!press(&mut desk, c));
        }
        assert!(
            view(&mut desk, 45, 18)
                .iter()
                .any(|line| line.contains("Resize"))
        );
        assert_eq!(desk.mode, Mode::Add("quote".into()));
        assert!(
            view(&mut desk, 100, 31)
                .iter()
                .any(|line| line.contains("quote"))
        );
        assert!(!desk.key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)));
        assert_eq!(desk.selected_box, INBOX);
        assert!(desk.entries[INBOX].iter().any(|item| item.title == "quote"));
        assert!(press(&mut desk, 'q'));
        assert!(
            !Desk::demo().entries[INBOX]
                .iter()
                .any(|item| item.title == "quote")
        );
    }
}
