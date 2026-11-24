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
}
impl Drop for Temp {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
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
