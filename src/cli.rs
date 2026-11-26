use std::{
    env,
    error::Error,
    ffi::OsString,
    fs,
    io::{IsTerminal, Read},
    path::PathBuf,
};

use serde::Serialize;

use crate::planning::PlanField;
use crate::store::{BOX_IDS, Item, NewItem, Sqlite, Store, StoreError};

type Result<T> = std::result::Result<T, Box<dyn Error>>;

const USAGE: &str = "lazygtd: standalone local GTD\n\
Usage: lazygtd [--demo | --db PATH] [COMMAND]\n\
No command: open TUI; --demo uses disposable in-memory data.
\
--agent-config PATH opens TUI with opt-in, confirmed remote dispatch.\n\
--watch-github-curl /absolute/path/to/curl enables unauthenticated public PR checks while TUI runs.\n\
--watch-github /absolute/path/to/gh uses an existing official GitHub CLI login instead.\n\
--codex-review [--approved-note PATH] adds interactive Codex app-server review to --agent-config.\n\
--claude-review shows live Claude questions/approval waits in Next Actions.\n\
Commands: add TITLE [BODY] [--box BOX] [--assignee me|agent] [--meta KEY=VALUE]...\n\
          edit-body ID REV (BODY | -) | retitle ID REV TITLE | move ID REV BOX\n\
          done ID REV | archive ID REV | reopen ID REV\n\
          list [--json] [--box BOX] [--assignee me|agent] [--done | --open] [--all]\n\
          show ID [--with conversation,steps]\n\
Agent: message ID REV TEXT | reply ID REV --as agent (TEXT | -) | conversation ID\n\
       assign ID REV me|agent | instruct ID REV TEXT | agent-status ID\n\
       agent-preview ID CONFIG | agent-run ID REV CONFIG --send\n\
       --demo agent-simulate ID REV\n\
Plan: meta ID REV FIELD VALUE | steps PROJECT | step-add PROJECT REV TITLE [BODY]\n\
      step-attach PROJECT REV ITEM ITEM_REV | step-detach PROJECT REV ITEM ITEM_REV\n\
      step-order PROJECT REV ITEM... | watch-resolve ID REV CLOSED_URL...
\
      Fields: due follow_up who then watch outcome short links since source\n\
Body: --body-file PATH or --body - (standard input) for add, edit-body, step-add, reply.\n\
Writes: --by NAME records the change as agent:NAME. -- ends options.\n\
Ask: questions [ID CHOICE] | question-import JSON | question-reply ID TEXT | question-reason ID TEXT\n\
Boxes: inbox, next, projects, waiting, someday, reference (review and desk are legacy IDs)\n\
Maintenance: update installs the latest published lazygtd release.\n\
Options: --help, --version";

/// Upper bound for a body read from a file or standard input; the store's own
/// limit is checked again after reading.
const BODY_LIMIT: u64 = 32_000;

enum Action<'a> {
    Meta(i64, i64, PlanField, &'a str),
    Steps(i64),
    StepAdd(i64, i64, &'a str, String),
    StepLink(bool, i64, i64, i64, i64),
    StepOrder(i64, i64, Vec<i64>),
    Calendar(i64, i64),
    Recover(i64, i64),
    Questions,
    QuestionImport(&'a str),
    AnswerText(i64, bool, &'a str),
    Watch(i64, i64, Vec<&'a str>),
    Answer(i64, u8),
    Tui(TuiOptions<'a>),
    Add(NewItem<'a>, String),
    EditBody(i64, i64, String),
    Retitle(i64, i64, &'a str),
    List(bool, ListFilter<'a>),
    Show(i64, With),
    Move(i64, i64, &'a str),
    Done(i64, i64),
    Archive(i64, i64),
    Reopen(i64, i64),
    Message(i64, i64, &'a str),
    Reply(i64, i64, String),
    Conversation(i64),
    Assign(i64, i64, &'a str),
    Instruct(i64, i64, &'a str),
    AgentStatus(i64),
    AgentPreview(i64, &'a str),
    AgentRun(i64, i64, &'a str),
    Simulate(i64, i64),
}

/// Options shared by commands. Only these exact spellings are options; any
/// other argument (or everything after `--`) stays positional.
#[derive(Default)]
struct Flags<'a> {
    seen: Vec<&'static str>,
    json: bool,
    by: Option<&'a str>,
    box_id: Option<&'a str>,
    assignee: Option<&'a str>,
    meta: Vec<&'a str>,
    body_file: Option<&'a str>,
    body_stdin: bool,
    as_role: Option<&'a str>,
    with: Option<&'a str>,
    all: bool,
    done: bool,
    open: bool,
}

const VALUE_FLAGS: [&str; 8] = [
    "--by",
    "--box",
    "--assignee",
    "--meta",
    "--body-file",
    "--body",
    "--as",
    "--with",
];
const SWITCHES: [&str; 4] = ["--json", "--all", "--done", "--open"];

impl<'a> Flags<'a> {
    fn split(args: &[&'a str]) -> Result<(Vec<&'a str>, Self)> {
        let mut flags = Self::default();
        let mut positional = Vec::new();
        let mut rest = args.iter();
        while let Some(&arg) = rest.next() {
            if arg == "--" {
                positional.extend(rest.by_ref().copied());
                break;
            }
            if let Some(&name) = SWITCHES.iter().find(|&&f| f == arg) {
                flags.mark(name)?;
                match name {
                    "--json" => flags.json = true,
                    "--all" => flags.all = true,
                    "--done" => flags.done = true,
                    _ => flags.open = true,
                }
            } else if let Some(&name) = VALUE_FLAGS.iter().find(|&&f| f == arg) {
                let value = *rest
                    .next()
                    .ok_or_else(|| format!("{name} needs a value; see --help"))?;
                if name != "--meta" {
                    flags.mark(name)?;
                }
                match name {
                    "--by" => flags.by = Some(value),
                    "--box" => flags.box_id = Some(value),
                    "--assignee" => flags.assignee = Some(value),
                    "--meta" => {
                        flags.seen.push(name);
                        flags.meta.push(value);
                    }
                    "--body-file" => flags.body_file = Some(value),
                    "--body" if value == "-" => flags.body_stdin = true,
                    "--body" => {
                        return Err("--body only accepts - (standard input); \
                                    pass text as an argument or use --body-file"
                            .into());
                    }
                    "--as" => flags.as_role = Some(value),
                    _ => flags.with = Some(value),
                }
            } else {
                positional.push(arg);
            }
        }
        Ok((positional, flags))
    }

    fn mark(&mut self, name: &'static str) -> Result<()> {
        if self.seen.contains(&name) {
            return Err(format!("{name} given more than once").into());
        }
        self.seen.push(name);
        Ok(())
    }

    /// Reject options the command does not take.
    fn allow(&self, command: &str, allowed: &[&str]) -> Result<()> {
        match self.seen.iter().find(|f| !allowed.contains(f)) {
            Some(flag) => Err(format!("{command} does not take {flag}; see --help").into()),
            None => Ok(()),
        }
    }

    fn actor(&self, default: &str) -> Result<String> {
        let Some(name) = self.by else {
            return Ok(default.into());
        };
        if name.is_empty()
            || name.len() > 64
            || !name
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
        {
            return Err("--by NAME must be 1–64 ASCII letters, digits, '-', '_' or '.'".into());
        }
        Ok(format!("agent:{name}"))
    }

    /// The body from the argument, `-`/`--body -` (standard input) or `--body-file`.
    fn body(&self, argument: Option<&str>, dash_reads_stdin: bool) -> Result<Option<String>> {
        let stdin = self.body_stdin || (dash_reads_stdin && argument == Some("-"));
        let argument = argument.filter(|a| !(dash_reads_stdin && *a == "-"));
        let sources = usize::from(stdin)
            + usize::from(self.body_file.is_some())
            + usize::from(argument.is_some());
        if sources > 1 {
            return Err(
                "give the body only once: as an argument, from - or with --body-file".into(),
            );
        }
        if stdin {
            return read_body(std::io::stdin().lock(), "standard input").map(Some);
        }
        if let Some(path) = self.body_file {
            return read_body(fs::File::open(path)?, path).map(Some);
        }
        Ok(argument.map(str::to_owned))
    }

    fn filter(&self) -> Result<ListFilter<'a>> {
        if let Some(b) = self.box_id
            && !BOX_IDS.contains(&b)
        {
            return Err("unknown box; see --help".into());
        }
        if let Some(a) = self.assignee
            && !["me", "agent"].contains(&a)
        {
            return Err("assignee must be me or agent".into());
        }
        if self.done && self.open {
            return Err("choose either --done or --open".into());
        }
        Ok(ListFilter {
            box_id: self.box_id,
            assignee: self.assignee,
            done: if self.done {
                Some(true)
            } else if self.open {
                Some(false)
            } else {
                None
            },
            all: self.all,
        })
    }

    fn meta(&self) -> Result<Vec<(PlanField, String)>> {
        let mut out: Vec<(PlanField, String)> = Vec::new();
        for pair in &self.meta {
            let (key, value) = pair
                .split_once('=')
                .ok_or("--meta takes KEY=VALUE; see --help")?;
            let field = PlanField::parse(key)?;
            // Links are one URL per line; repeating --meta links=URL adds a line.
            match out.iter_mut().find(|(f, _)| *f == field) {
                Some((PlanField::Links, joined)) => {
                    joined.push('\n');
                    joined.push_str(value);
                }
                _ => out.push((field, value.to_owned())),
            }
        }
        Ok(out)
    }
}

fn read_body(source: impl Read, name: &str) -> Result<String> {
    let mut bytes = Vec::new();
    source.take(BODY_LIMIT + 1).read_to_end(&mut bytes)?;
    if bytes.len() as u64 > BODY_LIMIT {
        return Err(format!("body from {name} exceeds {BODY_LIMIT} bytes").into());
    }
    String::from_utf8(bytes).map_err(|_| format!("body from {name} must be UTF-8").into())
}

#[derive(Default)]
struct ListFilter<'a> {
    box_id: Option<&'a str>,
    assignee: Option<&'a str>,
    done: Option<bool>,
    all: bool,
}

impl ListFilter<'_> {
    fn keeps(&self, item: &Item) -> bool {
        self.box_id.is_none_or(|b| item.box_id == b)
            && self.assignee.is_none_or(|a| item.assignee == a)
            && self.done.is_none_or(|d| item.done == d)
    }
}

#[derive(Default)]
struct With {
    conversation: bool,
    steps: bool,
}

impl With {
    fn parse(value: Option<&str>) -> Result<Self> {
        let mut with = Self::default();
        for part in value.into_iter().flat_map(|v| v.split(',')) {
            match part.trim() {
                "conversation" => with.conversation = true,
                "steps" => with.steps = true,
                _ => return Err("--with takes conversation and/or steps, comma-separated".into()),
            }
        }
        Ok(with)
    }
}

/// `show --with`: the item's own fields plus the requested lists.
#[derive(Serialize)]
struct Shown {
    #[serde(flatten)]
    item: Item,
    #[serde(skip_serializing_if = "Option::is_none")]
    conversation: Option<Vec<crate::talk::Message>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    steps: Option<Vec<crate::planning::Step>>,
}

fn print_json(value: &impl Serialize) -> Result<()> {
    println!("{}", serde_json::to_string(value)?);
    Ok(())
}

fn number(s: &str) -> Result<i64> {
    match s.parse::<i64>() {
        Ok(n) if n > 0 => Ok(n),
        _ => Err("ID and revision must be positive integers".into()),
    }
}

fn work_path() -> Result<PathBuf> {
    #[cfg(windows)]
    let base = env::var_os("LOCALAPPDATA").map(PathBuf::from);
    #[cfg(target_os = "macos")]
    let base = env::var_os("HOME").map(|p| PathBuf::from(p).join("Library/Application Support"));
    #[cfg(all(not(windows), not(target_os = "macos")))]
    let base = env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .or_else(|| env::var_os("HOME").map(|p| PathBuf::from(p).join(".local/share")));
    let base = base
        .filter(|p| p.is_absolute())
        .ok_or("no local profile path; use --db PATH or --demo")?;
    Ok(base.join("lazygtd").join("work.sqlite3"))
}

#[derive(Default)]
struct TuiOptions<'a> {
    agent: Option<&'a str>,
    gh: Option<&'a str>,
    curl: Option<&'a str>,
    review: Option<crate::review_agent::Provider>,
    note: Option<&'a str>,
}

fn open_tui<'a>(mut flags: &[&'a str], demo: bool) -> Result<Action<'a>> {
    if demo && !flags.is_empty() {
        return Err("external integrations are not available in demo".into());
    }
    let mut options = TuiOptions::default();
    while !flags.is_empty() {
        flags = match flags {
            ["--agent-config", value, rest @ ..] if options.agent.is_none() => {
                options.agent = Some(value);
                rest
            }
            ["--watch-github", value, rest @ ..]
                if options.gh.is_none() && options.curl.is_none() =>
            {
                options.gh = Some(value);
                rest
            }
            ["--watch-github-curl", value, rest @ ..]
                if options.gh.is_none() && options.curl.is_none() =>
            {
                options.curl = Some(value);
                rest
            }
            [flag @ ("--codex-review" | "--claude-review"), rest @ ..]
                if options.review.is_none() =>
            {
                use crate::review_agent::Provider;
                options.review = Some(if *flag == "--codex-review" {
                    Provider::Codex
                } else {
                    Provider::Claude
                });
                rest
            }
            ["--approved-note", value, rest @ ..] if options.note.is_none() => {
                options.note = Some(value);
                rest
            }
            _ => return Err("invalid or duplicate TUI options; see --help".into()),
        };
    }
    if (options.review.is_some() && options.agent.is_none())
        || (options.note.is_some() && options.review != Some(crate::review_agent::Provider::Codex))
    {
        return Err(
            "review requires --agent-config; --approved-note requires --codex-review".into(),
        );
    }
    if !std::io::stdin().is_terminal() || !std::io::stdout().is_terminal() {
        return Err("the TUI requires an interactive terminal; use --help for CLI commands".into());
    }
    Ok(Action::Tui(options))
}

pub fn run(args: &[OsString]) -> Result<()> {
    if matches!(args, [arg] if arg == "--help" || arg == "-h") {
        println!("{USAGE}");
        return Ok(());
    }
    if matches!(args, [arg] if arg == "--version" || arg == "-V") {
        println!("lazygtd {}", env!("CARGO_PKG_VERSION"));
        return Ok(());
    }
    if matches!(args, [arg] if arg == "update") {
        return crate::update::run();
    }
    let mut demo = false;
    let mut db = None;
    let rest = match args {
        [flag, rest @ ..] if flag == "--demo" => {
            demo = true;
            rest
        }
        [flag, path, rest @ ..] if flag == "--db" => {
            db = Some(PathBuf::from(path));
            rest
        }
        _ => args,
    };
    let rest = rest
        .iter()
        .map(|a| a.to_str().ok_or("command arguments must be valid UTF-8"))
        .collect::<std::result::Result<Vec<_>, _>>()?;
    let (action, actor) = if matches!(
        rest.as_slice(),
        [] | [
            "--agent-config"
                | "--watch-github"
                | "--watch-github-curl"
                | "--codex-review"
                | "--claude-review"
                | "--approved-note",
            ..,
        ]
    ) {
        (open_tui(&rest, demo)?, String::new())
    } else {
        parse(&rest, demo)?
    };
    let actor = actor.as_str();

    if matches!(action, Action::Tui(_))
        && !demo
        && let Ok(Some(version)) = crate::update::available()
        && crate::confirm_update(&version)?
    {
        eprintln!("lazygtd: {version} に更新しています…");
        match crate::update::install(&version) {
            Ok(executable) => return crate::update::restart(&executable, args),
            Err(error) => eprintln!("lazygtd: 更新できませんでした: {error}"),
        }
    }

    let mut profile_path = None;
    let mut store = if demo {
        Sqlite::rich_demo()?
    } else {
        let path = match db {
            Some(path) => path,
            None => {
                let path = work_path()?;
                if let Some(parent) = path.parent() {
                    crate::db_file::create_profile_dir(parent)?;
                }
                path
            }
        };
        let store = Sqlite::open(&path)?;
        profile_path = Some(path.canonicalize()?);
        store
    };
    match action {
        Action::Tui(options) => {
            let config = options
                .agent
                .map(|p| crate::agent::Config::load(std::path::Path::new(p)))
                .transpose()?;
            let watcher = match (options.gh, options.curl) {
                (Some(gh), None) => Some(crate::pr_watch::Watcher::new(std::path::Path::new(gh))?),
                (None, Some(curl)) => Some(crate::pr_watch::Watcher::new_curl(
                    std::path::Path::new(curl),
                )?),
                (None, None) => None,
                _ => return Err("choose exactly one GitHub watch transport".into()),
            };
            let review = if let Some(provider) = options.review {
                let config = config
                    .as_ref()
                    .ok_or("missing review agent configuration")?;
                Some(crate::review_agent::Settings::new(
                    config,
                    provider,
                    options.note.map(std::path::Path::new),
                )?)
            } else {
                None
            };
            crate::tui(Box::new(store), demo, profile_path, config, watcher, review)?;
        }
        Action::Add(new, body) => print_json(&store.create(&NewItem {
            body: &body,
            actor,
            ..new
        })?)?,
        Action::EditBody(id, rev, body) => print_json(&store.edit_body(id, rev, &body, actor)?)?,
        Action::Retitle(id, rev, title) => print_json(&store.retitle(id, rev, title, actor)?)?,
        Action::List(json, filter) => {
            let items = if filter.all {
                store.list_all()?
            } else {
                store.list()?
            };
            let items = items.into_iter().filter(|i| filter.keeps(i));
            if json {
                print_json(&items.collect::<Vec<_>>())?;
            } else {
                for item in items {
                    println!(
                        "{} r{} [{}{}{}] [{}] {}",
                        item.id,
                        item.revision,
                        item.box_id,
                        if item.done { " done" } else { "" },
                        if item.archived { " archived" } else { "" },
                        item.assignee,
                        item.title
                    );
                }
            }
        }
        Action::Show(id, with) => {
            let item = store.show(id)?;
            if !with.conversation && !with.steps {
                print_json(&item)?;
            } else {
                print_json(&Shown {
                    conversation: with
                        .conversation
                        .then(|| store.conversation(id))
                        .transpose()?,
                    steps: with.steps.then(|| store.steps(id)).transpose()?,
                    item,
                })?;
            }
        }
        Action::Move(id, rev, dest) => print_json(&store.move_to(id, rev, dest, actor)?)?,
        Action::Done(id, rev) => print_json(&store.done(id, rev, actor)?)?,
        Action::Archive(id, rev) => print_json(&store.archive(id, rev, actor)?)?,
        Action::Reopen(id, rev) => print_json(&store.reopen(id, rev, actor)?)?,
        Action::Meta(id, rev, field, value) => {
            print_json(&store.change_meta(id, rev, field, value, actor)?)?
        }
        Action::Steps(id) => {
            store.show(id)?;
            print_json(&store.steps(id)?)?;
        }
        Action::StepAdd(id, rev, title, body) => {
            print_json(&store.add_step(id, rev, title, &body, actor)?)?
        }
        Action::StepLink(attach, id, rev, step, step_rev) => {
            let item = if attach {
                store.attach(id, rev, step, step_rev, actor)?
            } else {
                store.detach(id, rev, step, step_rev, actor)?
            };
            print_json(&item)?;
        }
        Action::StepOrder(id, rev, order) => print_json(&store.reorder(id, rev, &order, actor)?)?,
        Action::Watch(id, rev, urls) => print_json(&store.watch_resolve(id, rev, &urls, actor)?)?,
        Action::QuestionImport(path) => {
            let mut bytes = Vec::new();
            fs::File::open(path)?.take(65_537).read_to_end(&mut bytes)?;
            if bytes.len() > 65_536 {
                return Err("question JSON exceeds 64 KiB".into());
            }
            let question: crate::questions::ImportQuestion = serde_json::from_slice(&bytes)?;
            println!(
                "{}",
                store.import_question(&question, crate::questions::now())?
            );
        }
        Action::AnswerText(id, reason, text) => store.answer(
            id,
            if reason {
                crate::questions::Answer::Reason(text)
            } else {
                crate::questions::Answer::Text(text)
            },
            crate::questions::now(),
        )?,
        Action::Calendar(id, rev) => print_json(&store.calendar_action(id, rev, actor)?)?,
        Action::Recover(id, run) => store.recover_agent(id, run)?,
        Action::Questions => print_json(&store.questions(crate::questions::now())?)?,
        Action::Answer(id, choice) => store.answer(
            id,
            crate::questions::Answer::Choice(choice),
            crate::questions::now(),
        )?,
        Action::Message(id, rev, body) => print_json(&store.send_message(id, rev, body, actor)?)?,
        Action::Reply(id, rev, body) => print_json(&store.reply(id, rev, &body, actor)?)?,
        Action::Conversation(id) => {
            store.show(id)?;
            print_json(&store.conversation(id)?)?;
        }
        Action::Assign(id, rev, who) => print_json(&store.assign(id, rev, who, actor)?)?,
        Action::Instruct(id, rev, instructions) => {
            print_json(&store.instruct(id, rev, instructions, actor)?)?
        }
        Action::AgentStatus(id) => {
            store.show(id)?;
            print_json(&store.latest_run(id)?)?;
        }
        Action::AgentPreview(id, path) => crate::agent::preview(
            &store,
            id,
            &crate::agent::Config::load(std::path::Path::new(path))?,
        )?,
        Action::AgentRun(id, rev, path) => crate::agent::run(
            &mut store,
            id,
            rev,
            &crate::agent::Config::load(std::path::Path::new(path))?,
        )?,
        Action::Simulate(id, rev) => {
            let item = store.show(id)?;
            if item.revision != rev {
                return Err(StoreError::Stale {
                    id,
                    expected: rev,
                    current: item.revision,
                }
                .into());
            }
            crate::agent::simulate(&mut store, &item)?;
            print_json(&store.latest_run(id)?)?;
        }
    }
    Ok(())
}

/// Parse a command into its action and the actor recorded for a write.
/// Bodies from files or standard input are read here, before the database opens.
fn parse<'a>(args: &[&'a str], demo: bool) -> Result<(Action<'a>, String)> {
    let (positional, flags) = Flags::split(args)?;
    let write = ["--by"];
    let body_flags = ["--by", "--body-file", "--body"];
    let command = positional.first().copied().unwrap_or_default();
    let allowed: &[&str] = match command {
        "add" => &[
            "--by",
            "--box",
            "--assignee",
            "--meta",
            "--body-file",
            "--body",
        ],
        "edit-body" | "step-add" => &body_flags,
        "reply" => &["--by", "--body-file", "--body", "--as"],
        "list" => &["--json", "--box", "--assignee", "--done", "--open", "--all"],
        "show" => &["--with"],
        "retitle" | "move" | "done" | "archive" | "reopen" | "meta" | "step-attach"
        | "step-detach" | "step-order" | "watch-resolve" | "calendar-action" | "message"
        | "assign" | "instruct" => &write,
        _ => &[],
    };
    flags.allow(command, allowed)?;
    let actor = flags.actor(match command {
        "watch-resolve" => "system:watch",
        "reply" => "agent",
        _ => "user",
    })?;
    let action = match positional.as_slice() {
        ["add", title, rest @ ..] if rest.len() <= 1 => Action::Add(
            NewItem {
                title,
                box_id: flags.box_id,
                assignee: flags.assignee,
                meta: flags.meta()?,
                ..NewItem::default()
            },
            flags
                .body(rest.first().copied(), false)?
                .unwrap_or_default(),
        ),
        ["edit-body", id, rev, rest @ ..] if rest.len() <= 1 => Action::EditBody(
            number(id)?,
            number(rev)?,
            flags
                .body(rest.first().copied(), true)?
                .ok_or("edit-body needs BODY, - or --body-file")?,
        ),
        ["retitle", id, rev, title] => Action::Retitle(number(id)?, number(rev)?, title),
        ["list"] => Action::List(flags.json, flags.filter()?),
        ["show", id] => Action::Show(number(id)?, With::parse(flags.with)?),
        ["move", id, rev, dest] if BOX_IDS.contains(dest) => {
            Action::Move(number(id)?, number(rev)?, dest)
        }
        ["done", id, rev] => Action::Done(number(id)?, number(rev)?),
        ["archive", id, rev] => Action::Archive(number(id)?, number(rev)?),
        ["reopen", id, rev] => Action::Reopen(number(id)?, number(rev)?),
        ["meta", id, rev, field, value] => {
            Action::Meta(number(id)?, number(rev)?, PlanField::parse(field)?, value)
        }
        ["steps", id] => Action::Steps(number(id)?),
        ["step-add", id, rev, title, rest @ ..] if rest.len() <= 1 => Action::StepAdd(
            number(id)?,
            number(rev)?,
            title,
            flags
                .body(rest.first().copied(), false)?
                .unwrap_or_default(),
        ),
        [
            cmd @ ("step-attach" | "step-detach"),
            id,
            rev,
            step,
            step_rev,
        ] => Action::StepLink(
            *cmd == "step-attach",
            number(id)?,
            number(rev)?,
            number(step)?,
            number(step_rev)?,
        ),
        ["step-order", id, rev, values @ ..] => Action::StepOrder(
            number(id)?,
            number(rev)?,
            values.iter().map(|x| number(x)).collect::<Result<_>>()?,
        ),
        ["watch-resolve", id, rev, urls @ ..] => {
            Action::Watch(number(id)?, number(rev)?, urls.to_vec())
        }
        ["calendar-action", id, rev] => Action::Calendar(number(id)?, number(rev)?),
        ["agent-recover", id, run, "--confirmed-stopped"] if !demo => {
            Action::Recover(number(id)?, number(run)?)
        }
        ["questions"] => Action::Questions,
        ["question-import", path] if !demo => Action::QuestionImport(path),
        [key @ ("question-reply" | "question-reason"), id, value] => {
            Action::AnswerText(number(id)?, *key == "question-reason", value)
        }
        ["questions", id, choice] => Action::Answer(number(id)?, choice.parse()?),
        ["message", id, rev, body] => Action::Message(number(id)?, number(rev)?, body),
        ["reply", id, rev, rest @ ..] if rest.len() <= 1 => {
            if flags.as_role != Some("agent") {
                return Err("reply needs --as agent".into());
            }
            Action::Reply(
                number(id)?,
                number(rev)?,
                flags
                    .body(rest.first().copied(), true)?
                    .ok_or("reply needs TEXT, - or --body-file")?,
            )
        }
        ["conversation", id] => Action::Conversation(number(id)?),
        ["assign", id, rev, who] => Action::Assign(number(id)?, number(rev)?, who),
        ["instruct", id, rev, instructions] => {
            Action::Instruct(number(id)?, number(rev)?, instructions)
        }
        ["agent-status", id] => Action::AgentStatus(number(id)?),
        ["agent-preview", id, config] if !demo => Action::AgentPreview(number(id)?, config),
        ["agent-run", id, rev, config, "--send"] if !demo => {
            Action::AgentRun(number(id)?, number(rev)?, config)
        }
        ["agent-simulate", id, rev] if demo => Action::Simulate(number(id)?, number(rev)?),
        _ => return Err("unsupported arguments; see --help".into()),
    };
    Ok((action, actor))
}

pub fn status(e: &(dyn Error + 'static)) -> u8 {
    if matches!(
        e.downcast_ref::<StoreError>(),
        Some(StoreError::Stale { .. })
    ) {
        3
    } else {
        1
    }
}
