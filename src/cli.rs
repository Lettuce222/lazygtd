use std::{
    env,
    error::Error,
    ffi::OsString,
    fs,
    io::{IsTerminal, Read},
    path::PathBuf,
};

use crate::store::{BOX_IDS, Sqlite, Store, StoreError};

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
Commands: add TITLE [BODY] | edit-body ID REV BODY | list [--json] | show ID | move ID REV BOX | done ID REV\n\
Agent: message ID REV TEXT | conversation ID | assign ID REV me|agent | instruct ID REV TEXT | agent-status ID\n\
       agent-preview ID CONFIG | agent-run ID REV CONFIG --send\n\
       --demo agent-simulate ID REV\n\
Plan: meta ID REV FIELD VALUE | steps PROJECT | step-add PROJECT REV TITLE [BODY]\n\
      step-attach PROJECT REV ITEM ITEM_REV | step-detach PROJECT REV ITEM ITEM_REV\n\
      step-order PROJECT REV ITEM... | watch-resolve ID REV CLOSED_URL...
\
Ask: questions [ID CHOICE] | question-import JSON | question-reply ID TEXT | question-reason ID TEXT\n\
Boxes: inbox, next, projects, waiting, someday, reference (review and desk are legacy IDs)\n\
Options: --help, --version";

enum Action<'a> {
    Meta(i64, i64, crate::planning::PlanField, &'a str),
    Steps(i64),
    StepAdd(i64, i64, &'a str, &'a str),
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
    Add(&'a str, &'a str),
    EditBody(i64, i64, &'a str),
    List(bool),
    Show(i64),
    Move(i64, i64, &'a str),
    Done(i64, i64),
    Message(i64, i64, &'a str),
    Conversation(i64),
    Assign(i64, i64, &'a str),
    Instruct(i64, i64, &'a str),
    AgentStatus(i64),
    AgentPreview(i64, &'a str),
    AgentRun(i64, i64, &'a str),
    Simulate(i64, i64),
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
    let action = match rest.as_slice() {
        []
        | [
            "--agent-config"
            | "--watch-github"
            | "--watch-github-curl"
            | "--codex-review"
            | "--claude-review"
            | "--approved-note",
            ..,
        ] => open_tui(&rest, demo)?,
        ["add", title] => Action::Add(title, ""),
        ["add", title, body] => Action::Add(title, body),
        ["edit-body", id, rev, body] => Action::EditBody(number(id)?, number(rev)?, body),
        ["list"] => Action::List(false),
        ["list", "--json"] => Action::List(true),
        ["show", id] => Action::Show(number(id)?),
        ["move", id, rev, dest] if BOX_IDS.contains(dest) => {
            Action::Move(number(id)?, number(rev)?, dest)
        }
        ["done", id, rev] => Action::Done(number(id)?, number(rev)?),
        ["meta", id, rev, field, value] => Action::Meta(
            number(id)?,
            number(rev)?,
            crate::planning::PlanField::parse(field)?,
            value,
        ),
        ["steps", id] => Action::Steps(number(id)?),
        ["step-add", id, rev, title] => Action::StepAdd(number(id)?, number(rev)?, title, ""),
        ["step-add", id, rev, title, body] => {
            Action::StepAdd(number(id)?, number(rev)?, title, body)
        }
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
        Action::Add(title, body) => println!(
            "{}",
            serde_json::to_string(&store.add_with_body(title, body, "user")?)?
        ),
        Action::EditBody(id, rev, body) => println!(
            "{}",
            serde_json::to_string(&store.edit_body(id, rev, body)?)?
        ),
        Action::List(true) => println!("{}", serde_json::to_string(&store.list()?)?),
        Action::List(false) => {
            for item in store.list()? {
                println!(
                    "{} r{} [{}{}] [{}] {}",
                    item.id,
                    item.revision,
                    item.box_id,
                    if item.done { " done" } else { "" },
                    item.assignee,
                    item.title
                );
            }
        }
        Action::Show(id) => println!("{}", serde_json::to_string(&store.show(id)?)?),
        Action::Move(id, rev, dest) => println!(
            "{}",
            serde_json::to_string(&store.move_to(id, rev, dest, "user")?)?
        ),
        Action::Meta(id, rev, field, value) => println!(
            "{}",
            serde_json::to_string(&store.change_meta(id, rev, field, value)?)?
        ),
        Action::Steps(id) => {
            store.show(id)?;
            println!("{}", serde_json::to_string(&store.steps(id)?)?);
        }
        Action::StepAdd(id, rev, title, body) => println!(
            "{}",
            serde_json::to_string(&store.add_step(id, rev, title, body)?)?
        ),
        Action::StepLink(attach, id, rev, step, step_rev) => {
            let item = if attach {
                store.attach(id, rev, step, step_rev)?
            } else {
                store.detach(id, rev, step, step_rev)?
            };
            println!("{}", serde_json::to_string(&item)?);
        }
        Action::StepOrder(id, rev, order) => println!(
            "{}",
            serde_json::to_string(&store.reorder(id, rev, &order)?)?
        ),
        Action::Watch(id, rev, urls) => println!(
            "{}",
            serde_json::to_string(&store.watch_resolve(id, rev, &urls)?)?
        ),
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
        Action::Calendar(id, rev) => println!(
            "{}",
            serde_json::to_string(&store.calendar_action(id, rev)?)?
        ),
        Action::Recover(id, run) => store.recover_agent(id, run)?,
        Action::Questions => println!(
            "{}",
            serde_json::to_string(&store.questions(crate::questions::now())?)?
        ),
        Action::Answer(id, choice) => store.answer(
            id,
            crate::questions::Answer::Choice(choice),
            crate::questions::now(),
        )?,
        Action::Message(id, rev, body) => println!(
            "{}",
            serde_json::to_string(&store.send_message(id, rev, body)?)?
        ),
        Action::Conversation(id) => {
            store.show(id)?;
            println!("{}", serde_json::to_string(&store.conversation(id)?)?);
        }
        Action::Assign(id, rev, who) => {
            println!("{}", serde_json::to_string(&store.assign(id, rev, who)?)?)
        }
        Action::Instruct(id, rev, instructions) => println!(
            "{}",
            serde_json::to_string(&store.instruct(id, rev, instructions)?)?
        ),
        Action::AgentStatus(id) => {
            store.show(id)?;
            println!("{}", serde_json::to_string(&store.latest_run(id)?)?);
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
            println!("{}", serde_json::to_string(&store.latest_run(id)?)?);
        }
        Action::Done(id, rev) => {
            println!("{}", serde_json::to_string(&store.done(id, rev, "user")?)?)
        }
    }
    Ok(())
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
