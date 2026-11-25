use std::{
    path::PathBuf,
    process::{Command, Output},
    sync::atomic::{AtomicUsize, Ordering},
};

use serde_json::Value;

struct Temp(PathBuf);
impl Temp {
    fn new() -> Self {
        static COUNTER: AtomicUsize = AtomicUsize::new(0);
        let id = COUNTER.fetch_add(1, Ordering::Relaxed);
        let unique = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let p =
            std::env::temp_dir().join(format!("lazygtd-test-{}-{id}-{unique}", std::process::id()));
        std::fs::create_dir(&p).unwrap();
        Self(p)
    }

    fn run(&self, args: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_lazygtd"))
            .env_clear()
            .env("HOME", &self.0)
            .env("LOCALAPPDATA", &self.0)
            .env("XDG_DATA_HOME", &self.0)
            .current_dir(&self.0)
            .args(args)
            .output()
            .unwrap()
    }

    fn run_stdin(&self, args: &[&str], input: &[u8]) -> Output {
        use std::io::Write;
        let mut child = Command::new(env!("CARGO_BIN_EXE_lazygtd"))
            .env_clear()
            .env("HOME", &self.0)
            .env("LOCALAPPDATA", &self.0)
            .env("XDG_DATA_HOME", &self.0)
            .current_dir(&self.0)
            .args(args)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .unwrap();
        child.stdin.take().unwrap().write_all(input).unwrap();
        child.wait_with_output().unwrap()
    }

    fn events(&self, id: i64) -> Vec<(i64, String, String)> {
        let c = rusqlite::Connection::open(self.0.join("agent.db")).unwrap();
        c.prepare("SELECT revision,kind,actor FROM event WHERE item_id=? ORDER BY revision")
            .unwrap()
            .query_map([id], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
            .unwrap()
            .collect::<rusqlite::Result<Vec<_>>>()
            .unwrap()
    }
}
impl Drop for Temp {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn fails(out: Output, message: &str) -> Output {
    assert!(!out.status.success(), "unexpected success");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains(message), "{stderr}");
    out
}

fn json(out: Output) -> Value {
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    serde_json::from_slice(&out.stdout).unwrap()
}

#[test]
fn help_version_and_invalid_use_do_not_create_profile() {
    let t = Temp::new();
    let version = t.run(&["--version"]);
    assert!(version.status.success());
    assert_eq!(
        String::from_utf8(version.stdout).unwrap().trim(),
        concat!("lazygtd ", env!("CARGO_PKG_VERSION"))
    );
    assert!(String::from_utf8_lossy(&t.run(&["--help"]).stdout).contains("--demo"));
    assert!(!t.run(&["--gtd-home", "no-such-directory"]).status.success());
    assert!(String::from_utf8_lossy(&t.run(&[]).stderr).contains("interactive terminal"));
    assert_eq!(std::fs::read_dir(&t.0).unwrap().count(), 0);
}

#[test]
fn independently_started_commands_persist_and_reject_stale_revisions() {
    let t = Temp::new();
    let add = json(t.run(&["add", "受け付けるq"]));
    assert_eq!(add["title"], "受け付けるq");
    assert_eq!(add["box_id"], "inbox");
    assert_eq!(add["revision"], 1);
    assert_eq!(json(t.run(&["list", "--json"])), serde_json::json!([add]));
    assert_eq!(json(t.run(&["show", "1"])), add);
    let moved = json(t.run(&["move", "1", "1", "next"]));
    assert_eq!(moved["revision"], 2);
    assert_eq!(moved["box_id"], "next");
    let stale = t.run(&["done", "1", "1"]);
    assert_eq!(stale.status.code(), Some(3));
    assert!(String::from_utf8_lossy(&stale.stderr).contains("current 2"));
    let done = json(t.run(&["done", "1", "2"]));
    assert_eq!(done["done"], true);
    assert_eq!(done["revision"], 3);
    assert_eq!(json(t.run(&["show", "1"])), done);
}

#[test]
fn demo_is_disposable_and_does_not_open_default_or_custom_storage() {
    let t = Temp::new();
    let initial = json(t.run(&["--demo", "list", "--json"]));
    let examples = initial.as_array().unwrap();
    assert_eq!(examples.len(), 46);
    let boxes: std::collections::BTreeSet<_> = examples
        .iter()
        .map(|item| item["box_id"].as_str().unwrap())
        .collect();
    assert_eq!(
        boxes,
        [
            "review",
            "desk",
            "inbox",
            "next",
            "projects",
            "waiting",
            "someday",
            "reference"
        ]
        .into_iter()
        .collect()
    );
    json(t.run(&["--demo", "add", "discard me"]));
    json(t.run(&["--demo", "done", "1", "2"]));
    assert_eq!(json(t.run(&["--demo", "list", "--json"])), initial);
    assert_eq!(std::fs::read_dir(&t.0).unwrap().count(), 0);
    assert_eq!(
        json(t.run(&["--db", "separate.db", "list", "--json"])),
        serde_json::json!([])
    );
    assert!(t.0.join("separate.db").is_file());
}

#[cfg(unix)]
#[test]
fn agent_provider_switch_progress_and_handback_without_network() {
    use serde_json::{Value, json};
    use std::{
        fs,
        io::Write,
        os::unix::fs::PermissionsExt,
        process::{Command, Stdio},
        thread,
        time::Duration,
    };
    let dir = std::env::temp_dir().join(format!("lazygtd-agents-{}", std::process::id()));
    fs::create_dir(&dir).unwrap();
    let db = dir.join("test.sqlite");
    let config = dir.join("agent.json");
    for (provider, start, finish) in [
        (
            "codex",
            json!({"type":"item.started","item":{"type":"command_execution"}}),
            vec![
                json!({"type":"item.completed","item":{"type":"agent_message","text":"prepared report"}}),
                json!({"type":"turn.completed"}),
            ],
        ),
        (
            "claude",
            json!({"type":"assistant","message":{"content":[{"type":"tool_use","name":"Read"}]}}),
            vec![
                json!({"type":"result","subtype":"success","is_error":false,"result":"prepared report","permission_denials":[]}),
            ],
        ),
    ] {
        let file = dir.join(provider);
        let mut fixture = fs::File::create(&file).unwrap();
        writeln!(fixture, "#!/bin/sh").unwrap();
        writeln!(fixture,"input=$(cat); [ \"$input\" = \"$(printf 'Task: Try the preview\\n\\n- list three options')\" ] || exit 9").unwrap();
        let flag = if provider == "codex" {
            "read-only"
        } else {
            "Read,Glob,Grep"
        };
        writeln!(
            fixture,
            "case \" $* \" in *' {flag} '*) ;; *) exit 10 ;; esac"
        )
        .unwrap();
        writeln!(fixture, "printf '%s\\n' '{start}'").unwrap();
        writeln!(
            fixture,
            "while [ ! -e release-{provider} ]; do sleep 0.03; done"
        )
        .unwrap();
        for event in finish {
            writeln!(fixture, "printf '%s\\n' '{event}'").unwrap();
        }
        drop(fixture);
        fs::set_permissions(file, fs::Permissions::from_mode(0o700)).unwrap();
        fs::write(
            &config,
            json!({"provider":provider,"workspace":dir,"executable":dir.join(provider),"allow_file_reads":true})
                .to_string(),
        )
        .unwrap();
        let path = format!(
            "{}:{}",
            dir.display(),
            std::env::var("PATH").unwrap_or_default()
        );
        let cmd = |args: &[&str]| {
            let mut command = Command::new(env!("CARGO_BIN_EXE_lazygtd"));
            command.arg("--db").arg(&db).args(args).env("PATH", &path);
            command
        };
        let call = |args: &[&str]| {
            let out = cmd(args).output().unwrap();
            assert!(
                out.status.success(),
                "{args:?}: {}",
                String::from_utf8_lossy(&out.stderr)
            );
            serde_json::from_slice::<Value>(&out.stdout).unwrap()
        };
        let item = call(&["add", "Try the preview"]);
        let id = item["id"].as_i64().unwrap().to_string();
        call(&["assign", &id, "1", "agent"]);
        call(&["instruct", &id, "2", "- list three options"]);
        let config_path = config.to_str().unwrap();
        let preview = call(&["agent-preview", &id, config_path]);
        assert_eq!(preview["provider"], provider);
        assert!(call(&["agent-status", &id]).is_null());
        assert!(
            !cmd(&["agent-run", &id, "3", config_path])
                .output()
                .unwrap()
                .status
                .success()
        );
        let child = cmd(&["agent-run", &id, "3", config_path, "--send"])
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let mut reading = false;
        for _ in 0..100 {
            if call(&["agent-status", &id])["activity"] == "reading" {
                reading = true;
                break;
            }
            thread::sleep(Duration::from_millis(20));
        }
        let duplicate = cmd(&["agent-run", &id, "3", config_path, "--send"])
            .output()
            .unwrap();
        assert!(!duplicate.status.success());
        assert!(String::from_utf8_lossy(&duplicate.stderr).contains("an agent run is active"));
        fs::write(dir.join(format!("release-{provider}")), []).unwrap();
        let out = child.wait_with_output().unwrap();
        assert!(
            out.status.success(),
            "{:?}",
            String::from_utf8_lossy(&out.stderr)
        );
        assert!(reading);
        let status = call(&["agent-status", &id]);
        assert_eq!(status["state"], "review");
        assert_eq!(status["result"], "prepared report");
        let item = call(&["show", &id]);
        assert_eq!(item["box_id"], "review");
        assert_eq!(item["assignee"], "me");
        assert_eq!(item["done"], false);
        assert_eq!(item["revision"], 4);
    }
    fs::remove_dir_all(dir).unwrap();
}

#[cfg(unix)]
#[test]
fn default_db_is_private_and_rejects_public_files_and_symlinks() {
    use std::os::unix::fs::{PermissionsExt, symlink};
    let t = Temp::new();
    json(t.run(&["add", "private body"]));
    #[cfg(target_os = "macos")]
    let root =
        t.0.join("Library")
            .join("Application Support")
            .join("lazygtd");
    #[cfg(not(target_os = "macos"))]
    let root = t.0.join("lazygtd");
    let db = root.join("work.sqlite3");
    assert_eq!(
        std::fs::metadata(&root).unwrap().permissions().mode() & 0o777,
        0o700
    );
    assert_eq!(
        std::fs::metadata(&db).unwrap().permissions().mode() & 0o777,
        0o600
    );
    let link = t.0.join("link.sqlite3");
    symlink(&db, &link).unwrap();
    assert!(
        !t.run(&["--db", link.to_str().unwrap(), "list"])
            .status
            .success()
    );
    std::fs::set_permissions(&db, std::fs::Permissions::from_mode(0o644)).unwrap();
    assert!(!t.run(&["list"]).status.success());
}

#[cfg(unix)]
#[test]
fn database_can_live_under_os_ancestor_alias_but_not_a_symlinked_leaf() {
    use std::os::unix::fs::symlink;
    let t = Temp::new();
    let real = t.0.join("real");
    std::fs::create_dir_all(real.join("db")).unwrap();
    symlink(&real, t.0.join("alias")).unwrap();
    let aliased = t.0.join("alias/db/task.sqlite");
    json(t.run(&[
        "--db",
        aliased.to_str().unwrap(),
        "add",
        "path normalization",
    ]));
    let real_leaf = real.join("db/task.sqlite");
    let malicious = t.0.join("other.sqlite");
    symlink(&real_leaf, &malicious).unwrap();
    assert!(
        !t.run(&["--db", malicious.to_str().unwrap(), "list"])
            .status
            .success()
    );
}

const DB: [&str; 2] = ["--db", "agent.db"];

fn db<'a>(args: &[&'a str]) -> Vec<&'a str> {
    DB.iter().chain(args).copied().collect()
}

#[test]
fn add_creates_box_assignee_and_planning_at_once_or_nothing() {
    let t = Temp::new();
    let item = json(t.run(&db(&[
        "add",
        "wait for the reviewer",
        "--box",
        "waiting",
        "--assignee",
        "agent",
        "--meta",
        "who=reviewer",
        "--meta",
        "since=2026-01-02",
        "--meta",
        "source=https://example.invalid/thread/1",
        "--meta",
        "links=https://example.invalid/a",
        "--meta",
        "links=https://example.invalid/b",
        "--by",
        "importer",
    ])));
    assert_eq!(item["revision"], 1);
    assert_eq!(item["box_id"], "waiting");
    assert_eq!(item["assignee"], "agent");
    assert_eq!(item["archived"], false);
    assert_eq!(item["planning"]["who"], "reviewer");
    assert_eq!(item["planning"]["since"], "2026-01-02");
    assert_eq!(
        item["planning"]["source"],
        "https://example.invalid/thread/1"
    );
    assert_eq!(
        item["planning"]["links"],
        serde_json::json!(["https://example.invalid/a", "https://example.invalid/b"])
    );
    assert_eq!(t.events(1), [(1, "add".into(), "agent:importer".into())]);

    for (args, message) in [
        (
            vec!["add", "x", "--box", "next", "--meta", "who=someone"],
            "waiting fields",
        ),
        (
            vec!["add", "x", "--meta", "due=2026-02-30"],
            "calendar date",
        ),
        (
            vec!["add", "x", "--meta", "nonsense=1"],
            "unknown planning field",
        ),
        (vec!["add", "x", "--meta", "due"], "KEY=VALUE"),
        (vec!["add", "x", "--box", "nowhere"], "unknown box"),
        (vec!["add", "x", "--assignee", "robot"], "assignee"),
        (vec!["add", "x", "--by", "has space"], "--by NAME"),
        (vec!["add", "x", "--by", "a", "--by", "b"], "more than once"),
        (vec!["add", "x", "--with", "steps"], "does not take --with"),
        (vec!["add", "x", "--body", "text"], "--body only accepts -"),
        (vec!["add", "x", "body", "--body-file", "f"], "only once"),
    ] {
        fails(t.run(&db(&args)), message);
    }
    assert_eq!(
        json(t.run(&db(&["list", "--json", "--all"])))
            .as_array()
            .unwrap()
            .len(),
        1
    );
}

#[test]
fn bodies_from_file_and_stdin_match_the_argument() {
    let t = Temp::new();
    let body = "## 見出し\n\n- \"quoted\" and 'single' $HOME `tick`\n- 二行目\n\n```sh\necho -n --body\n```\n";
    std::fs::write(t.0.join("body.md"), body).unwrap();
    let from_arg = json(t.run(&db(&["add", "arg", body])));
    let from_file = json(t.run(&db(&["add", "file", "--body-file", "body.md"])));
    let from_stdin = json(t.run_stdin(&db(&["add", "stdin", "--body", "-"]), body.as_bytes()));
    assert_eq!(from_arg["body"], body);
    assert_eq!(from_file["body"], body);
    assert_eq!(from_stdin["body"], body);

    let edited = json(t.run_stdin(&db(&["edit-body", "1", "1", "-"]), b"new\r\nbody"));
    assert_eq!(edited["body"], "new\nbody");
    assert_eq!(edited["revision"], 2);
    let edited = json(t.run(&db(&[
        "edit-body",
        "1",
        "2",
        "--body-file",
        "body.md",
        "--by",
        "bot",
    ])));
    assert_eq!(edited["body"], body);
    let edited = json(t.run(&db(&["edit-body", "1", "3", "plain argument"])));
    assert_eq!(edited["body"], "plain argument");
    fails(t.run(&db(&["edit-body", "1", "4"])), "edit-body needs");
    fails(t.run(&db(&["add", "x", "--body-file", "missing.md"])), "");
    std::fs::write(t.0.join("big.md"), "a".repeat(32_001)).unwrap();
    fails(
        t.run(&db(&["add", "x", "--body-file", "big.md"])),
        "exceeds 32000",
    );
    fails(
        t.run_stdin(&db(&["add", "x", "--body", "-"]), &[0xff, 0xfe]),
        "UTF-8",
    );

    let project = json(t.run(&db(&["add", "project", "--box", "projects"])));
    let id = project["id"].as_i64().unwrap().to_string();
    json(t.run_stdin(
        &db(&["step-add", &id, "1", "step", "--body", "-"]),
        body.as_bytes(),
    ));
    let step = &json(t.run(&db(&["show", &id, "--with", "steps"])))["steps"][0];
    let step = json(t.run(&db(&["show", &step["id"].as_i64().unwrap().to_string()])));
    assert_eq!(step["body"], body);
    assert_eq!(
        t.events(1).into_iter().map(|e| e.2).collect::<Vec<_>>(),
        ["user", "user", "agent:bot", "user"]
    );
}

#[test]
fn retitle_archive_reopen_and_reply_check_revisions() {
    let t = Temp::new();
    json(t.run(&db(&["add", "draft title"])));
    assert_eq!(
        t.run(&db(&["retitle", "1", "2", "x"])).status.code(),
        Some(3)
    );
    let item = json(t.run(&db(&["retitle", "1", "1", "final title", "--by", "bot"])));
    assert_eq!(
        (item["title"].as_str(), item["revision"].as_i64()),
        (Some("final title"), Some(2))
    );

    fails(t.run(&db(&["reply", "1", "2", "text"])), "--as agent");
    fails(
        t.run(&db(&["reply", "1", "2", "--as", "user", "text"])),
        "--as agent",
    );
    assert_eq!(
        t.run(&db(&["reply", "1", "1", "--as", "agent", "x"]))
            .status
            .code(),
        Some(3)
    );
    let item = json(t.run_stdin(
        &db(&["reply", "1", "2", "--as", "agent", "-", "--by", "bot"]),
        "調べた結果です。\n".as_bytes(),
    ));
    assert_eq!(item["unread_reply"], true);
    assert_eq!(item["pending_message"], false);
    let shown = json(t.run(&db(&["show", "1", "--with", "conversation"])));
    assert_eq!(shown["conversation"][0]["role"], "agent");
    assert_eq!(shown["conversation"][0]["body"], "調べた結果です。");
    assert_eq!(shown["conversation"][0]["read"], false);
    assert!(shown["conversation"][0]["run_id"].is_null());
    assert!(shown.get("steps").is_none());
    assert_eq!(shown["title"], "final title");

    assert_eq!(t.run(&db(&["archive", "1", "2"])).status.code(), Some(3));
    let item = json(t.run(&db(&["archive", "1", "3", "--by", "bot"])));
    assert_eq!(item["archived"], true);
    assert_eq!(item["done"], false);
    assert_eq!(json(t.run(&db(&["list", "--json"]))), serde_json::json!([]));
    assert_eq!(json(t.run(&db(&["list", "--json", "--all"])))[0]["id"], 1);
    assert!(String::from_utf8_lossy(&t.run(&db(&["list", "--all"])).stdout).contains("archived"));
    fails(t.run(&db(&["retitle", "1", "4", "again"])), "archived");
    assert_eq!(t.run(&db(&["reopen", "1", "3"])).status.code(), Some(3));
    let item = json(t.run(&db(&["reopen", "1", "4"])));
    assert_eq!(item["archived"], false);
    assert_eq!(item["box_id"], "inbox");
    json(t.run(&db(&["move", "1", "5", "next"])));
    json(t.run(&db(&["done", "1", "6"])));
    let item = json(t.run(&db(&["reopen", "1", "7", "--by", "bot"])));
    assert_eq!(
        (item["done"].as_bool(), item["box_id"].as_str()),
        (Some(false), Some("next"))
    );
    assert_eq!(
        t.events(1),
        [
            (1, "add".into(), "user".into()),
            (2, "retitle".into(), "agent:bot".into()),
            (3, "reply".into(), "agent:bot".into()),
            (4, "archive".into(), "agent:bot".into()),
            (5, "reopen".into(), "user".into()),
            (6, "move".into(), "user".into()),
            (7, "done".into(), "user".into()),
            (8, "reopen".into(), "agent:bot".into()),
        ]
    );
}

#[test]
fn list_filters_and_show_with_steps_and_conversation() {
    let t = Temp::new();
    json(t.run(&db(&["add", "inbox mine"])));
    json(t.run(&db(&[
        "add",
        "next agent",
        "--box",
        "next",
        "--assignee",
        "agent",
    ])));
    json(t.run(&db(&["add", "next mine", "--box", "next"])));
    json(t.run(&db(&["done", "3", "1"])));
    json(t.run(&db(&["add", "archived", "--box", "next"])));
    json(t.run(&db(&["archive", "4", "1"])));
    let ids = |args: &[&str]| {
        let mut all = vec!["list", "--json"];
        all.extend(args);
        json(t.run(&db(&all)))
            .as_array()
            .unwrap()
            .iter()
            .map(|i| i["id"].as_i64().unwrap())
            .collect::<Vec<_>>()
    };
    assert_eq!(ids(&[]), [1, 2, 3]);
    assert_eq!(ids(&["--all"]), [1, 2, 3, 4]);
    assert_eq!(ids(&["--box", "next"]), [2, 3]);
    assert_eq!(ids(&["--box", "next", "--all"]), [2, 3, 4]);
    assert_eq!(ids(&["--assignee", "agent"]), [2]);
    assert_eq!(ids(&["--done"]), [3]);
    assert_eq!(ids(&["--open"]), [1, 2]);
    assert_eq!(ids(&["--open", "--all"]), [1, 2, 4]);
    fails(
        t.run(&db(&["list", "--done", "--open"])),
        "either --done or --open",
    );
    fails(t.run(&db(&["list", "--box", "desk2"])), "unknown box");
    fails(t.run(&db(&["list", "--by", "bot"])), "does not take --by");

    let project = json(t.run(&db(&["add", "project", "--box", "projects"])));
    let id = project["id"].as_i64().unwrap().to_string();
    json(t.run(&db(&["step-add", &id, "1", "first", "--by", "bot"])));
    json(t.run(&db(&["message", &id, "2", "please plan"])));
    let shown = json(t.run(&db(&["show", &id, "--with", "conversation,steps"])));
    assert_eq!(shown["id"], project["id"]);
    assert_eq!(shown["revision"], 3);
    assert_eq!(shown["steps"][0]["title"], "first");
    assert_eq!(shown["steps"][0]["state"], "next");
    assert_eq!(shown["conversation"][0]["role"], "user");
    assert_eq!(shown["conversation"][0]["body"], "please plan");
    assert_eq!(json(t.run(&db(&["show", &id]))).get("steps"), None);
    fails(
        t.run(&db(&["show", &id, "--with", "events"])),
        "--with takes",
    );
    // `--` keeps option-like text positional.
    let odd = json(t.run(&db(&["add", "--", "--json"])));
    assert_eq!(odd["title"], "--json");
}

#[cfg(unix)]
#[test]
fn trusted_launch_uses_normal_settings_rules_file_and_is_announced() {
    use serde_json::json;
    use std::{fs, os::unix::fs::PermissionsExt};
    let t = Temp::new();
    let root = t.0.canonicalize().unwrap();
    let workspace = root.join("ws");
    fs::create_dir(&workspace).unwrap();
    let record = root.join("args.txt");
    let exe = root.join("claude");
    fs::write(
        &exe,
        format!(
            "#!/bin/sh\ncat >/dev/null\n: > {rec}\nfor a in \"$@\"; do printf '%s\\n' \"$a\" >> {rec}; done\nprintf 'mark=%s\\n' \"${{LAZYGTD_FIXTURE_MARK:-}}\" >> {rec}\nprintf '%s\\n' '{{\"type\":\"result\",\"subtype\":\"success\",\"is_error\":false,\"result\":\"synthetic report\",\"permission_denials\":[]}}'\n",
            rec = record.display()
        ),
    )
    .unwrap();
    fs::set_permissions(&exe, fs::Permissions::from_mode(0o700)).unwrap();
    let rules = root.join("rules.md");
    fs::write(&rules, "- synthetic rule\n").unwrap();
    fs::set_permissions(&rules, fs::Permissions::from_mode(0o600)).unwrap();
    let db = root.join("agent.db");
    let config = root.join("agent.json");
    let lazygtd = |args: &[&str]| {
        Command::new(env!("CARGO_BIN_EXE_lazygtd"))
            .env_clear()
            .env("HOME", &root)
            .env("LAZYGTD_FIXTURE_MARK", "inherited")
            .arg("--db")
            .arg(&db)
            .args(args)
            .output()
            .unwrap()
    };
    let write_config = |value: Value| fs::write(&config, value.to_string()).unwrap();
    let config_arg = config.to_str().unwrap();
    let base = json!({"provider":"claude","executable":exe,"workspace":workspace});
    let run = |rev: i64| -> Vec<String> {
        let item = json(lazygtd(&["add", "Synthetic task", "--assignee", "agent"]));
        let id = item["id"].to_string();
        json(lazygtd(&[
            "instruct",
            &id,
            &rev.to_string(),
            "synthetic directions",
        ]));
        json(lazygtd(&[
            "agent-run",
            &id,
            &(rev + 1).to_string(),
            config_arg,
            "--send",
        ]));
        fs::read_to_string(&record)
            .unwrap()
            .lines()
            .map(str::to_owned)
            .collect()
    };

    write_config(base.clone());
    let readonly = run(1);
    assert!(readonly.iter().any(|a| a == "--safe-mode"));
    assert!(readonly.iter().any(|a| a == "--setting-sources"));
    assert!(readonly.iter().any(|a| a == "mark="));
    let item = json(lazygtd(&["list", "--json"]))[0]["id"].to_string();
    let preview = json(lazygtd(&["agent-preview", &item, config_arg]));
    assert_eq!(preview["mode"], "read-only");
    assert!(preview["trusted_warning"].is_null());

    let mut trusted = base.clone();
    trusted["mode"] = json!("trusted");
    trusted["permission_mode"] = json!("acceptEdits");
    trusted["rules_file"] = json!(rules);
    trusted["inherit_env"] = json!(true);
    write_config(trusted.clone());
    let preview = json(lazygtd(&["agent-preview", &item, config_arg]));
    assert_eq!(preview["mode"], "trusted");
    assert_eq!(preview["permission_mode"], "acceptEdits");
    assert_eq!(preview["rules_file"], json!(rules));
    assert_eq!(preview["inherit_env"], true);
    assert!(
        preview["trusted_warning"]
            .as_str()
            .unwrap()
            .contains("hooks")
    );
    let args = run(1);
    for restricted in [
        "--safe-mode",
        "--setting-sources",
        "--tools",
        "--strict-mcp-config",
        "--disallowedTools",
    ] {
        assert!(
            !args.iter().any(|a| a == restricted),
            "{restricted}: {args:?}"
        );
    }
    assert!(
        args.windows(2)
            .any(|w| w[0] == "--permission-mode" && w[1] == "acceptEdits")
    );
    assert!(
        args.windows(2)
            .any(|w| w[0] == "--append-system-prompt-file" && w[1] == rules.to_str().unwrap())
    );
    assert!(args.iter().any(|a| a == "mark=inherited"));

    // Without inherit_env the environment is still narrowed.
    trusted["inherit_env"] = json!(false);
    write_config(trusted.clone());
    assert!(run(1).iter().any(|a| a == "mark="));

    let rejected = |value: Value, message: &str| {
        write_config(value);
        fails(lazygtd(&["agent-preview", &item, config_arg]), message);
    };
    fs::set_permissions(&rules, fs::Permissions::from_mode(0o620)).unwrap();
    rejected(trusted.clone(), "rules_file must be owned by you");
    fs::set_permissions(&rules, fs::Permissions::from_mode(0o600)).unwrap();
    let mut relative = trusted.clone();
    relative["rules_file"] = json!("rules.md");
    rejected(relative, "rules_file must be an absolute");
    let mut bypass = trusted.clone();
    bypass["permission_mode"] = json!("bypassPermissions");
    rejected(bypass, "permission_mode for claude must be one of");
    let mut readonly_rules = base.clone();
    readonly_rules["rules_file"] = json!(rules);
    rejected(readonly_rules, "require mode: trusted");
    let mut codex_rules = trusted.clone();
    codex_rules["provider"] = json!("codex");
    codex_rules["permission_mode"] = json!("workspace-write");
    rejected(codex_rules, "rules_file is supported for claude only");

    write_config(trusted);
    fails(
        t.run(&["--demo", "--agent-config", config_arg]),
        "not available in demo",
    );
    fails(t.run(&["--demo", "agent-preview", "1", config_arg]), "");
}
