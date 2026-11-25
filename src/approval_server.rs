//! Opt-in Codex app-server stdio adapter. Provider approvals are denied, not
//! translated into locally approved Git/OS commands or persistent grants.
use serde_json::{Value, json};
use std::{
    collections::HashSet,
    env,
    fs::{self, File},
    io::{self, BufRead, BufReader, Read, Write},
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    sync::{
        atomic::{AtomicU64, Ordering},
        mpsc,
    },
    thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

const MAX_LINE: u64 = 1_048_576;
const MAX_FRAMES: usize = 16_384;
const TIMEOUT: Duration = Duration::from_secs(600);

const NOTE_TOOL: &str = "read_approved_note";
const NOTE_LIMIT: u64 = 8192;

pub struct ApprovedNote {
    path: PathBuf,
    content: String,
}

impl ApprovedNote {
    pub fn load(path: &Path) -> io::Result<Self> {
        if !local_note_path(path) {
            return Err(fail("approved note requires a local absolute UTF-8 path"));
        }
        let meta = fs::symlink_metadata(path)?;
        if !meta.is_file() || meta.file_type().is_symlink() || meta.len() > NOTE_LIMIT {
            return Err(fail(
                "approved note must be an existing regular file up to 8192 bytes",
            ));
        }
        let path = path.canonicalize()?;
        if !local_note_path(&path) {
            return Err(fail("approved note resolves to an unsafe display path"));
        }
        let mut bytes = Vec::new();
        let file = File::open(&path)?;
        let actual = file.metadata()?;
        if !actual.is_file() || actual.len() > NOTE_LIMIT {
            return Err(fail("approved note changed or exceeds 8192 bytes"));
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            if meta.dev() != actual.dev() || meta.ino() != actual.ino() {
                return Err(fail("approved note changed while opening"));
            }
        }
        file.take(NOTE_LIMIT + 1).read_to_end(&mut bytes)?;
        if bytes.len() as u64 > NOTE_LIMIT {
            return Err(fail("approved note exceeds 8192 bytes"));
        }
        let content = String::from_utf8(bytes).map_err(|_| fail("approved note is not UTF-8"))?;
        if content.trim().is_empty() || !content.lines().all(display_safe) {
            return Err(fail(
                "approved note is empty or contains unsafe display characters",
            ));
        }
        Ok(Self { path, content })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }
    pub fn content(&self) -> &str {
        &self.content
    }
}

fn local_note_path(path: &Path) -> bool {
    if !path.is_absolute()
        || !path
            .to_str()
            .is_some_and(|s| display_safe(s) && !s.contains('\t'))
    {
        return false;
    }
    #[cfg(windows)]
    {
        matches!(path.components().next(), Some(std::path::Component::Prefix(p))
            if matches!(p.kind(), std::path::Prefix::Disk(_) | std::path::Prefix::VerbatimDisk(_)))
    }
    #[cfg(not(windows))]
    true
}

fn display_safe(s: &str) -> bool {
    !s.chars().any(|c| (c.is_control() && c != '\t')
        || matches!(c, '\u{061c}' | '\u{200b}'..='\u{200f}' | '\u{2028}'..='\u{202e}' | '\u{2060}'..='\u{2069}' | '\u{feff}'))
}

fn note_result(id: &Value, approved: Option<&ApprovedNote>) -> Value {
    match approved {
        Some(note) => json!({"id":id,"result":{"success":true,
            "contentItems":[{"type":"inputText","text":note.content()}]}}),
        None => json!({"id":id,"result":{"success":false,
            "contentItems":[{"type":"inputText","text":"No note was approved for this call."}]}}),
    }
}

fn fail(msg: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, msg)
}

fn timed_out() -> io::Error {
    io::Error::new(io::ErrorKind::TimedOut, "Codex app-server timed out")
}

#[derive(Default)]
struct Scope {
    thread_id: Option<String>,
    turn_id: Option<String>,
}

impl Scope {
    fn matches(&self, params: &Value) -> bool {
        self.thread_id
            .as_deref()
            .is_some_and(|id| params["threadId"].as_str() == Some(id))
            && self
                .turn_id
                .as_deref()
                .is_some_and(|id| params["turnId"].as_str() == Some(id))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReviewDecision {
    Deny,
    CancelTurn,
    AllowNote,
}

#[derive(Debug, Clone)]
pub struct Review {
    pub method: String,
    pub detail: Option<String>,
}

impl Review {
    fn matching(message: &Value, scope: &Scope) -> Option<Self> {
        let params = &message["params"];
        if !scope.matches(params) {
            return None;
        }
        let method = message["method"].as_str()?;
        if !matches!(
            method,
            "item/commandExecution/requestApproval"
                | "item/fileChange/requestApproval"
                | "item/permissions/requestApproval"
                | "item/tool/call"
        ) {
            return None;
        }
        let item = if method == "item/tool/call" {
            "callId"
        } else {
            "itemId"
        };
        id(&params[item]).ok()?;
        Some(Self {
            method: method.to_owned(),
            detail: None,
        })
    }
}

/// Builds a response to exactly the server request id, never to an unsolicited
/// narrative or opaque command string. A request from a different run can
/// never be accepted; its own JSON-RPC request gets an explicit denial.
fn deny(message: &Value, cancel: bool) -> Option<Value> {
    let method = message["method"].as_str()?;
    let id = message.get("id")?;
    if !(id.is_string() || id.as_i64().is_some() || id.as_u64().is_some()) {
        return None;
    }
    Some(match method {
        "item/commandExecution/requestApproval" | "item/fileChange/requestApproval" => {
            json!({"id":id,"result":{"decision":if cancel {"cancel"} else {"decline"}}})
        }
        "item/permissions/requestApproval" => {
            json!({"id":id,"result":{"permissions":{},"scope":"turn"}})
        }
        "mcpServer/elicitation/request" => {
            json!({"id":id,"result":{"action":"decline","content":null}})
        }
        "item/tool/call" => note_result(id, None),
        _ => json!({"id":id,"error":{"code":-32601,"message":"Unsupported server request"}}),
    })
}

pub(crate) struct Io {
    child: Child,
    sink: mpsc::SyncSender<Value>,
    stream: mpsc::Receiver<io::Result<Value>>,
    deadline: Instant,
    frames: usize,
}

impl Drop for Io {
    fn drop(&mut self) {
        // The CLI is created as a separate process group on Unix; best effort
        // termination while the group leader is still ours. Windows currently
        // only guarantees an attempt to terminate this child process.
        #[cfg(unix)]
        if matches!(self.child.try_wait(), Ok(None))
            && let Ok(group) = i32::try_from(self.child.id())
        {
            unsafe extern "C" {
                fn killpg(pgrp: i32, sig: i32) -> i32;
            }
            // SAFETY: positive ID obtained from the live, owned child.
            unsafe {
                killpg(group, 9);
            }
        }
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

impl Io {
    pub(crate) fn open(cmd: &mut Command, budget: Duration) -> io::Result<Self> {
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt;
            cmd.process_group(0);
        }
        let mut child = cmd
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()?;
        let mut input = child.stdin.take().expect("piped");
        let stdout = child.stdout.take().expect("piped");
        let (sink, out) = mpsc::sync_channel::<Value>(8);
        thread::spawn(move || {
            while let Ok(value) = out.recv() {
                if serde_json::to_writer(&mut input, &value).is_err()
                    || input.write_all(b"\n").and_then(|_| input.flush()).is_err()
                {
                    break;
                }
            }
        });
        let (send, stream) = mpsc::sync_channel(8);
        thread::spawn(move || {
            let mut reader = BufReader::new(stdout);
            loop {
                let mut line = Vec::new();
                let result = match (&mut reader)
                    .take(MAX_LINE + 1)
                    .read_until(b'\n', &mut line)
                {
                    Ok(0) => break,
                    Ok(n) if n as u64 <= MAX_LINE && line.last() == Some(&b'\n') => {
                        serde_json::from_slice(&line).map_err(|_| fail("invalid app-server JSON"))
                    }
                    Ok(_) => Err(fail("incomplete/oversized app-server frame")),
                    Err(e) => Err(e),
                };
                let end = result.is_err();
                if send.send(result).is_err() || end {
                    break;
                }
            }
        });
        Ok(Self {
            child,
            sink,
            stream,
            deadline: Instant::now() + budget,
            frames: 0,
        })
    }

    pub(crate) fn send(&self, value: Value) -> io::Result<()> {
        if Instant::now() >= self.deadline {
            return Err(timed_out());
        }
        self.sink
            .try_send(value)
            .map_err(|_| fail("app-server input closed or overloaded"))
    }

    pub(crate) fn receive(&mut self) -> io::Result<Value> {
        self.frames += 1;
        if self.frames > MAX_FRAMES {
            return Err(fail("too many app-server frames"));
        }
        let left = self
            .deadline
            .checked_duration_since(Instant::now())
            .ok_or_else(timed_out)?;
        self.stream.recv_timeout(left).map_err(|e| match e {
            mpsc::RecvTimeoutError::Timeout => timed_out(),
            mpsc::RecvTimeoutError::Disconnected => fail("app-server closed before completion"),
        })?
    }

    fn reply_to_server_request(&self, message: &Value) -> io::Result<bool> {
        if message.get("method").is_some() && message.get("id").is_some() {
            let reply = deny(message, false).ok_or_else(|| fail("invalid server request id"))?;
            self.send(reply)?;
            return Ok(true);
        }
        Ok(false)
    }

    fn wait_response(&mut self, id: i64) -> io::Result<Value> {
        loop {
            let value = self.receive()?;
            if self.reply_to_server_request(&value)? {
                continue;
            }
            if value["id"].as_i64() == Some(id) {
                return value
                    .get("result")
                    .cloned()
                    .ok_or_else(|| fail("app-server rejected request"));
            }
        }
    }
}

static HOME_SEQUENCE: AtomicU64 = AtomicU64::new(0);
pub(crate) struct IsolatedHome(pub(crate) PathBuf);

impl IsolatedHome {
    pub(crate) fn create() -> io::Result<Self> {
        #[cfg(unix)]
        let base = env::temp_dir();
        #[cfg(windows)]
        let base = env::var_os("LOCALAPPDATA")
            .map(PathBuf::from)
            .filter(|p| local_note_path(p))
            .ok_or_else(|| fail("LOCALAPPDATA must be a local absolute directory"))?;
        let base = base.canonicalize()?;
        if !local_note_path(&base) {
            return Err(fail("app-server temporary directory is not a local path"));
        }
        let stamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        for _ in 0..64 {
            let sequence = HOME_SEQUENCE.fetch_add(1, Ordering::Relaxed);
            let path = base.join(format!(
                "lazygtd-codex-home-{}-{stamp:x}-{sequence:x}",
                std::process::id()
            ));
            match crate::local_files::create_private(&path) {
                Ok(()) => return Ok(Self(path)),
                Err(e) if e.kind() == io::ErrorKind::AlreadyExists => continue,
                Err(e) => return Err(e),
            }
        }
        Err(fail(
            "could not allocate a fresh private app-server config directory",
        ))
    }
}

impl Drop for IsolatedHome {
    fn drop(&mut self) {
        if self.0.canonicalize().ok().as_ref() == Some(&self.0) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }
}

/// The reviewer must return promptly or observe its absolute deadline. No
/// response from this callback can grant execution, network, or write access.
pub fn run_with_review(
    command: &mut Command,
    input: &str,
    workspace: &Path,
    model: Option<&str>,
    progress: impl FnMut(&str) -> io::Result<()>,
    note: Option<&ApprovedNote>,
    reviewer: impl FnMut(&Review, Instant) -> ReviewDecision,
) -> io::Result<String> {
    if !command
        .get_envs()
        .any(|(key, value)| key == "OPENAI_API_KEY" && value.is_some_and(|value| !value.is_empty()))
    {
        return Err(fail(
            "app-server requires OPENAI_API_KEY; existing Codex login files are deliberately not read",
        ));
    }
    let home = IsolatedHome::create()?;
    command
        .env("CODEX_HOME", &home.0)
        .env("HOME", &home.0)
        .env("USERPROFILE", &home.0)
        .env("XDG_CONFIG_HOME", &home.0)
        .env("XDG_RUNTIME_DIR", &home.0)
        .env("XDG_DATA_HOME", &home.0)
        .env("XDG_STATE_HOME", &home.0)
        .env("XDG_CACHE_HOME", &home.0);
    // Keep this adapter separate from exec/resume. The caller chooses an
    // absolute trusted CLI, enables dispatch and supplies its scrubbed env.
    command.args([
        "-c",
        "model_provider=\"openai\"",
        "-c",
        "analytics.enabled=false",
        "-c",
        "check_for_update_on_startup=false",
        "-c",
        "mcp_servers={}",
        "-c",
        "web_search=\"disabled\"",
        "-c",
        "features.hooks=false",
        "-c",
        "features.plugins=false",
        "-c",
        "features.apps=false",
        "-c",
        "features.shell_tool=false",
        "-c",
        "features.view_image=false",
        "-c",
        "features.shell_snapshot=false",
        "-c",
        "features.skill_mcp_dependency_install=false",
        "-c",
        "skills.bundled.enabled=false",
        "-c",
        "skills.include_instructions=false",
        "-c",
        "project_doc_max_bytes=0",
        "-c",
        "shell_environment_policy.inherit=\"none\"",
        "app-server",
        "--listen",
        "stdio://",
    ]);
    drive(
        command.current_dir(workspace),
        RunSpec {
            input,
            workspace,
            model,
            budget: TIMEOUT,
            note,
        },
        progress,
        reviewer,
    )
}

fn id(value: &Value) -> io::Result<String> {
    value
        .as_str()
        .filter(|s| !s.is_empty() && s.len() <= 256 && !s.chars().any(char::is_control))
        .map(str::to_owned)
        .ok_or_else(|| fail("app-server did not provide a valid scope id"))
}

struct RunSpec<'a> {
    input: &'a str,
    workspace: &'a Path,
    model: Option<&'a str>,
    budget: Duration,
    note: Option<&'a ApprovedNote>,
}

fn drive(
    command: &mut Command,
    spec: RunSpec<'_>,
    mut progress: impl FnMut(&str) -> io::Result<()>,
    mut reviewer: impl FnMut(&Review, Instant) -> ReviewDecision,
) -> io::Result<String> {
    let RunSpec {
        input,
        workspace,
        model,
        budget,
        note,
    } = spec;
    if input.trim().is_empty() || input.len() > 100_000 {
        return Err(fail("invalid app-server prompt length"));
    }
    let workspace = workspace
        .to_str()
        .ok_or_else(|| fail("workspace must be UTF-8"))?;
    let mut io = Io::open(command, budget)?;
    io.send(json!({"id":1,"method":"initialize","params":{
        "clientInfo":{"name":"lazygtd","version":env!("CARGO_PKG_VERSION")},
        "capabilities":{"experimentalApi":note.is_some()}
    }}))?;
    io.wait_response(1)?;
    io.send(json!({"method":"initialized"}))?;
    let mut thread_params = json!({
        "cwd":workspace,"model":model,"modelProvider":"openai",
        "approvalPolicy":"on-request","sandbox":"read-only","ephemeral":true
    });
    if note.is_some() {
        thread_params["dynamicTools"] = json!([{
            "type":"function","name":NOTE_TOOL,
            "description":"Ask the human to send the exact preselected text note once. Takes no arguments.",
            "inputSchema":{"type":"object","properties":{},"additionalProperties":false}
        }]);
    }
    io.send(json!({"id":2,"method":"thread/start","params":thread_params}))?;
    let thread_id = id(&io.wait_response(2)?["thread"]["id"])?;
    progress("connected")?;
    io.send(json!({"id":3,"method":"turn/start","params":{
        "threadId":thread_id,"input":[{"type":"text","text":input,"text_elements":[]}]
    }}))?;
    let turn_id = id(&io.wait_response(3)?["turn"]["id"])?;
    let scope = Scope {
        thread_id: Some(thread_id),
        turn_id: Some(turn_id),
    };
    progress("working")?;
    let mut report = None;
    let mut denied = false;
    let mut seen = HashSet::new();
    let mut offered_note = false;
    loop {
        let value = io.receive()?;
        let params = &value["params"];
        if value.get("method").is_some() && value.get("id").is_some() {
            let handle = &value["id"];
            if !(handle.is_string() || handle.as_i64().is_some() || handle.as_u64().is_some()) {
                return Err(fail("invalid server request id"));
            }
            let request =
                serde_json::to_string(handle).map_err(|_| fail("invalid server request id"))?;
            if request.len() > 512 || !seen.insert(request) {
                return Err(fail("app-server reused a live server request ID"));
            }
            let mut decision = ReviewDecision::Deny;
            let note_call = value["method"] == "item/tool/call";
            let eligible_note = note_call
                && !offered_note
                && note.is_some()
                && params["tool"] == NOTE_TOOL
                && params.get("namespace").is_none_or(Value::is_null)
                && params["arguments"]
                    .as_object()
                    .is_some_and(serde_json::Map::is_empty)
                && scope.matches(params);
            if let Some(review) =
                Review::matching(&value, &scope).filter(|_| !note_call || eligible_note)
            {
                if note_call {
                    offered_note = true;
                }
                let deadline = (Instant::now() + Duration::from_secs(180)).min(io.deadline);
                decision = reviewer(&review, deadline);
                if Instant::now() >= deadline {
                    decision = ReviewDecision::Deny;
                }
                let allowed = eligible_note && decision == ReviewDecision::AllowNote;
                progress(if allowed {
                    "approved_note_authorized"
                } else {
                    "approval_denied"
                })?;
                denied |= !allowed;
            }
            let reply = if eligible_note && offered_note && decision == ReviewDecision::AllowNote {
                note_result(&value["id"], note)
            } else {
                deny(&value, decision == ReviewDecision::CancelTurn)
                    .ok_or_else(|| fail("invalid server request id"))?
            };
            io.send(reply)?;
            if decision == ReviewDecision::CancelTurn {
                io.send(json!({"id":4,"method":"turn/interrupt","params":{
                    "threadId":scope.thread_id,"turnId":scope.turn_id
                }}))?;
                io.wait_response(4)?;
                return Err(io::Error::new(
                    io::ErrorKind::Interrupted,
                    "provider turn canceled by reviewer",
                ));
            }
            continue;
        }
        match value["method"].as_str() {
            Some("item/completed")
                if scope.matches(params) && params["item"]["type"] == "agentMessage" =>
            {
                report = params["item"]["text"].as_str().map(|text| {
                    text.chars()
                        .filter(|&ch| !ch.is_control() || matches!(ch, '\n' | '\t'))
                        .take(12_000)
                        .collect::<String>()
                });
            }
            Some("turn/completed")
                if params["threadId"].as_str() == scope.thread_id.as_deref()
                    && params["turn"]["id"].as_str() == scope.turn_id.as_deref() =>
            {
                if params["turn"]["status"] != "completed" {
                    return Err(fail("Codex app-server turn did not complete"));
                }
                let report = report
                    .filter(|s| !s.trim().is_empty())
                    .ok_or_else(|| fail("Codex app-server produced no report"))?;
                return Ok(if denied {
                    format!("A provider permission request was denied.\n\n{report}")
                } else {
                    report
                });
            }
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn definite_denials_preserve_json_rpc_ids_and_never_grant() {
        for (method, result) in [
            (
                "item/permissions/requestApproval",
                json!({"permissions":{},"scope":"turn"}),
            ),
            (
                "item/commandExecution/requestApproval",
                json!({"decision":"decline"}),
            ),
            (
                "item/fileChange/requestApproval",
                json!({"decision":"decline"}),
            ),
            (
                "mcpServer/elicitation/request",
                json!({"action":"decline","content":null}),
            ),
        ] {
            let request = json!({"id":"fixture","method":method,"params":{"threadId":"A","turnId":"B","itemId":"C"}});
            assert_eq!(
                deny(&request, false).unwrap(),
                json!({"id":"fixture","result":result})
            );
        }
        assert!(
            deny(
                &json!({"id":null,"method":"item/fileChange/requestApproval"}),
                false
            )
            .is_none()
        );
        assert_eq!(
            deny(&json!({"id":42,"method":"unknown"}), false).unwrap()["error"]["code"],
            -32601
        );
    }

    #[cfg(unix)]
    #[test]
    fn frozen_note_is_approved_once_after_exact_call_and_never_reopened() {
        let home = IsolatedHome::create().unwrap();
        let path = home.0.join("fixture.txt");
        fs::write(&path, "benign frozen note").unwrap();
        let note = ApprovedNote::load(&path).unwrap();
        fs::write(&path, "benign subsequently changed note").unwrap();
        assert_eq!(note.path(), path);
        let program = r#"#!/bin/sh
read line
printf '%s\n' '{"id":1,"result":{}}'
read line
read line
printf '%s\n' '{"id":2,"result":{"thread":{"id":"t"}}}'
read line
printf '%s\n' '{"id":3,"result":{"turn":{"id":"v"}}}'
printf '%s\n' '{"id":8,"method":"item/tool/call","params":{"threadId":"other","turnId":"v","callId":"foreign","tool":"read_approved_note","arguments":{}}}'
IFS= read -r reply
case "$reply" in *'"success":false'*) ;; *) exit 8;; esac
printf '%s\n' '{"id":9,"method":"item/tool/call","params":{"threadId":"t","turnId":"v","callId":"bad-args","tool":"read_approved_note","arguments":{"path":"fixture.txt"}}}'
IFS= read -r reply
case "$reply" in *'"success":false'*) ;; *) exit 9;; esac
printf '%s\n' '{"id":10,"method":"item/tool/call","params":{"threadId":"t","turnId":"v","callId":"C","tool":"read_approved_note","arguments":{}}}'
IFS= read -r reply
test "$reply" = '{"id":10,"result":{"contentItems":[{"text":"benign frozen note","type":"inputText"}],"success":true}}' || exit 10
printf '%s\n' '{"id":11,"method":"item/tool/call","params":{"threadId":"t","turnId":"v","callId":"repeated","tool":"read_approved_note","arguments":{}}}'
IFS= read -r reply
case "$reply" in *'"success":false'*) ;; *) exit 11;; esac
printf '%s\n' '{"method":"item/completed","params":{"threadId":"t","turnId":"v","item":{"type":"agentMessage","text":"offline fixture done"}}}'
printf '%s\n' '{"method":"turn/completed","params":{"threadId":"t","turn":{"id":"v","status":"completed"}}}'
"#;
        let mut reviews = 0;
        let report = drive(
            Command::new("/bin/sh").args(["-c", program]),
            RunSpec {
                input: "synthetic prompt only",
                workspace: &home.0,
                model: None,
                budget: Duration::from_secs(3),
                note: Some(&note),
            },
            |_| Ok(()),
            |review, deadline| {
                reviews += 1;
                assert_eq!(review.method, "item/tool/call");
                assert!(deadline > Instant::now());
                ReviewDecision::AllowNote
            },
        )
        .unwrap();
        assert_eq!(reviews, 1);
        assert_eq!(report, "offline fixture done");
    }

    #[cfg(unix)]
    #[test]
    fn explicit_cancel_returns_correlated_decision_and_interrupts_only_active_turn() {
        let peer = r#"
read line
printf '%s\n' '{"id":1,"result":{}}'
read line
read line
printf '%s\n' '{"id":2,"result":{"thread":{"id":"T"}}}'
read line
printf '%s\n' '{"id":3,"result":{"turn":{"id":"V"}}}'
printf '%s\n' '{"id":12,"method":"item/fileChange/requestApproval","params":{"threadId":"T","turnId":"V","itemId":"synthetic"}}'
IFS= read -r reply
test "$reply" = '{"id":12,"result":{"decision":"cancel"}}' || exit 2
IFS= read -r interrupt
test "$interrupt" = '{"id":4,"method":"turn/interrupt","params":{"threadId":"T","turnId":"V"}}' || exit 3
printf '%s\n' '{"id":4,"result":{}}'
"#;
        let err = drive(
            Command::new("/bin/sh").args(["-c", peer]),
            RunSpec {
                input: "synthetic prompt only",
                workspace: Path::new("/tmp"),
                model: None,
                budget: Duration::from_secs(3),
                note: None,
            },
            |_| Ok(()),
            |review, _| {
                assert_eq!(review.method, "item/fileChange/requestApproval");
                ReviewDecision::CancelTurn
            },
        )
        .unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::Interrupted);
    }

    #[cfg(unix)]
    #[test]
    fn fake_stdio_peer_observes_denial_and_reports_only_matching_turn() {
        // A cooperative offline fixture. No network, credentials, or provider.
        let program = r#"#!/bin/sh
read line
printf '%s\n' '{"id":1,"result":{}}'
read line
read line
printf '%s\n' '{"id":2,"result":{"thread":{"id":"test-thread"}}}'
read line
printf '%s\n' '{"id":3,"result":{"turn":{"id":"test-turn"}}}'
printf '%s\n' '{"id":"request-7","method":"item/permissions/requestApproval","params":{"threadId":"test-thread","turnId":"test-turn","itemId":"i","permissions":{}}}'
IFS= read -r reply
test "$reply" = '{"id":"request-7","result":{"permissions":{},"scope":"turn"}}' || exit 3
printf '%s\n' '{"method":"item/completed","params":{"threadId":"test-thread","turnId":"test-turn","item":{"type":"agentMessage","text":"result from fixture"}}}'
printf '%s\n' '{"method":"turn/completed","params":{"threadId":"different-thread","turn":{"id":"test-turn","status":"failed"}}}'
printf '%s\n' '{"method":"turn/completed","params":{"threadId":"test-thread","turn":{"id":"test-turn","status":"completed"}}}'
"#;
        let mut progress = Vec::new();
        let result = drive(
            Command::new("/bin/sh").args(["-c", program]),
            RunSpec {
                input: "synthetic request",
                workspace: Path::new("/tmp"),
                model: None,
                budget: Duration::from_secs(3),
                note: None,
            },
            |p| {
                progress.push(p.to_owned());
                Ok(())
            },
            |review, deadline| {
                assert_eq!(review.method, "item/permissions/requestApproval");
                assert!(deadline > Instant::now());
                ReviewDecision::Deny
            },
        )
        .unwrap();
        assert!(result.ends_with("result from fixture"));
        assert_eq!(progress, ["connected", "working", "approval_denied"]);
    }
}
