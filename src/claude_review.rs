//! Explicit, deny-only Claude Code review over the SDK's stream-json control protocol.
use crate::approval_server::{Io, IsolatedHome, Review, ReviewDecision};
use serde_json::{Value, json};
use std::{
    collections::HashSet,
    io,
    path::Path,
    process::Command,
    time::{Duration, Instant},
};

const TOOL: &str = "AskUserQuestion";
const BUDGET: Duration = Duration::from_secs(600);
const WAIT: Duration = Duration::from_secs(180);

fn fail(message: &'static str) -> io::Error {
    io::Error::other(message)
}

pub(crate) fn run_with_review(
    command: &mut Command,
    input: &str,
    _workspace: &Path,
    model: Option<&str>,
    mut progress: impl FnMut(&str) -> io::Result<()>,
    mut reviewer: impl FnMut(&Review, Instant) -> ReviewDecision,
) -> io::Result<String> {
    if input.len() > 128 * 1024 {
        return Err(fail("Claude review input exceeds 128 KiB"));
    }
    if !command
        .get_envs()
        .any(|(k, v)| k == "ANTHROPIC_API_KEY" && v.is_some_and(|s| !s.is_empty()))
    {
        return Err(fail("Claude review requires ANTHROPIC_API_KEY"));
    }
    let home = IsolatedHome::create()?;
    command.current_dir(&home.0);
    for key in [
        "HOME",
        "USERPROFILE",
        "CLAUDE_CONFIG_DIR",
        "XDG_CONFIG_HOME",
        "XDG_DATA_HOME",
        "XDG_STATE_HOME",
        "XDG_CACHE_HOME",
        "XDG_RUNTIME_DIR",
    ] {
        command.env(key, &home.0);
    }
    command.args([
        "--print",
        "--input-format",
        "stream-json",
        "--output-format",
        "stream-json",
        "--verbose",
        "--permission-prompt-tool",
        "stdio",
        "--permission-mode",
        "default",
        "--safe-mode",
        "--setting-sources",
        "",
        "--strict-mcp-config",
        "--mcp-config",
        r#"{"mcpServers":{}}"#,
        "--no-session-persistence",
        "--tools",
        TOOL,
        "--max-turns",
        "4",
        "--settings",
        r#"{"disableAllHooks":true,"permissions":{"ask":["AskUserQuestion"]}}"#,
    ]);
    if let Some(model) = model {
        command.args(["--model", model]);
    }
    let mut rpc = Io::open(command, BUDGET)?;
    let deadline = Instant::now() + BUDGET;
    drive(&mut rpc, input, deadline, &mut progress, &mut reviewer)
}

fn reply(id: &str, result: Value) -> Value {
    json!({"type":"control_response","response":{"subtype":"success","request_id":id,"response":result}})
}

fn wire_id(v: &Value) -> io::Result<&str> {
    v.as_str()
        .filter(|s| {
            !s.is_empty()
                && s.len() <= 160
                && s.bytes()
                    .all(|c| c.is_ascii_alphanumeric() || matches!(c, b'_' | b'-'))
        })
        .ok_or_else(|| fail("invalid Claude control identifier"))
}

// Only single-line questions are shown, with a strict size and no control
// or formatting characters. Never use model text as instructions or a tool ID.
fn summary(input: &Value) -> Option<String> {
    let questions = input["questions"].as_array()?;
    if questions.is_empty() || questions.len() > 4 {
        return None;
    }
    let mut lines = Vec::new();
    for q in questions {
        let value = q["question"].as_str()?;
        if value.len() > 512 {
            return None;
        }
        let mut line = String::new();
        for ch in value.chars() {
            if ch == ' '
                || (!ch.is_whitespace()
                    && (ch.escape_debug().eq(std::iter::once(ch))
                        || matches!(ch, '\'' | '"' | '\\')))
            {
                line.push(ch);
            } else {
                return None;
            }
        }
        let line = line.trim();
        if line.is_empty() || matches!(line, "." | "..") {
            return None;
        }
        lines.push(line.to_owned());
    }
    Some(lines.join(" / "))
}

fn drive(
    rpc: &mut Io,
    input: &str,
    deadline: Instant,
    progress: &mut impl FnMut(&str) -> io::Result<()>,
    reviewer: &mut impl FnMut(&Review, Instant) -> ReviewDecision,
) -> io::Result<String> {
    const INIT: &str = "lazygtd-init";
    rpc.send(json!({"type":"control_request","request_id":INIT,"request":{"subtype":"initialize","hooks":null}}))?;
    loop {
        let v = rpc.receive()?;
        if v["type"] == "control_request" {
            return Err(fail("unexpected Claude request before initialization"));
        }
        if v["type"] == "control_response" && v["response"]["request_id"] == INIT {
            if v["response"]["subtype"] != "success" {
                return Err(fail("Claude rejected review initialization"));
            }
            break;
        }
    }
    progress("Claude review connected")?;
    rpc.send(
        json!({"type":"user","session_id":"","parent_tool_use_id":null,
        "message":{"role":"user","content":[{"type":"text","text":input}]}}),
    )?;
    let mut session: Option<String> = None;
    let mut ids = HashSet::new();
    let mut calls = HashSet::new();
    loop {
        let v = rpc.receive()?;
        match v["type"].as_str() {
            Some("system") if v["subtype"] == "init" => {
                let tools = v["tools"]
                    .as_array()
                    .ok_or_else(|| fail("Claude did not declare its tool roster"))?;
                if session.is_some()
                    || !tools.iter().any(|t| t == TOOL)
                    || tools.iter().any(|t| t != TOOL && t != "EndConversation")
                    || v["mcp_servers"].as_array().is_none_or(|s| !s.is_empty())
                {
                    return Err(fail(
                        "Claude review requires only the question tool and no MCP servers",
                    ));
                }
                session = Some(wire_id(&v["session_id"])?.to_owned());
            }
            Some("control_request") => {
                let id = wire_id(&v["request_id"])?;
                if !ids.insert(id.to_owned()) {
                    return Err(fail("duplicate Claude request identifier"));
                }
                let req = &v["request"];
                if req["subtype"] != "can_use_tool" {
                    return Err(fail("unsupported Claude control request"));
                }
                let key = wire_id(&req["tool_use_id"])?;
                let scope_ok = session
                    .as_deref()
                    .is_some_and(|session| v.get("session_id").is_none_or(|id| id == session));
                let fresh = calls.insert(key.to_owned());
                let safe = scope_ok
                    && fresh
                    && req["tool_name"] == TOOL
                    && req["input"].is_object()
                    && req.get("agent_id").is_none_or(Value::is_null);
                let cancel = if safe {
                    let until = deadline.min(Instant::now() + WAIT);
                    let decision = reviewer(
                        &Review {
                            method: "Claude: AskUserQuestion".into(),
                            detail: summary(&req["input"]),
                        },
                        until,
                    );
                    matches!(decision, ReviewDecision::CancelTurn) && Instant::now() < until
                } else {
                    false
                };
                rpc.send(reply(id, json!({"behavior":"deny","message":"Not authorized in lazygtd review","interrupt":cancel})))?;
                if cancel {
                    // Await the interrupted result/exit before tearing the pipe down; ensure a late
                    // result cannot turn cancellation into a successful application report.
                    loop {
                        match rpc.receive() {
                            Ok(v) if v["type"] != "result" && v["type"] != "control_request" => {}
                            _ => return Err(fail("Claude turn interrupted by operator")),
                        }
                    }
                }
            }
            Some("result") => {
                if !session
                    .as_deref()
                    .is_some_and(|session| v["session_id"] == session)
                    || v["subtype"] != "success"
                    || v["is_error"] != false
                {
                    return Err(fail("Claude review did not complete successfully"));
                }
                return v["result"]
                    .as_str()
                    .map(str::to_owned)
                    .ok_or_else(|| fail("Claude returned no text result"));
            }
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn display_is_plain_single_line_and_unicode() {
        assert_eq!(
            summary(&json!({"questions":[{"question":"次の手順は何ですか？"}]})).as_deref(),
            Some("次の手順は何ですか？")
        );
        for question in [
            "",
            "x\nFake",
            "x\u{1b}[2J",
            "x\u{202e}name",
            "x\u{200b}name",
        ] {
            assert_eq!(summary(&json!({"questions":[{"question":question}]})), None);
        }
    }

    #[cfg(unix)]
    #[test]
    fn sdk_control_frames_are_correlated_and_never_allow() {
        let peer = r#"
import json, sys
def get():
    return json.loads(sys.stdin.readline())
def put(frame):
    print(json.dumps(frame),flush=True)
assert get()['request']['subtype']=='initialize'
put({'type':'control_response','response':{'subtype':'success','request_id':'lazygtd-init'}})
assert get()['message']['content'][0]['text']=='test fixture'
put({'type':'system','subtype':'init','session_id':'s1','tools':['AskUserQuestion'],'mcp_servers':[]})
def ask(rid,tid,tool,session='s1'):
    put({'type':'control_request','request_id':rid,'session_id':session,'request':{'subtype':'can_use_tool','tool_name':tool,'tool_use_id':tid,'input':{'questions':[{'question':'Continue?'}]}}})
    res = get()['response']
    assert res['request_id']==rid
    assert res['response']['behavior']=='deny'
    return res['response']['interrupt']
assert not ask('b1','t1','Bash')
assert not ask('q1','t2','AskUserQuestion','other-session')
assert not ask('q2','t3','AskUserQuestion')
assert not ask('q3','t3','AskUserQuestion')
assert ask('q4','t4','AskUserQuestion')
put({'type':'result','subtype':'success','session_id':'s1','is_error':False,'result':'cannot override cancellation'})
"#;
        let mut rpc = Io::open(
            Command::new("python3").args(["-c", peer]),
            Duration::from_secs(2),
        )
        .unwrap();
        let mut count = 0;
        let result = drive(
            &mut rpc,
            "test fixture",
            Instant::now() + Duration::from_secs(2),
            &mut |_| Ok(()),
            &mut |review: &Review, _| {
                assert_eq!(review.detail.as_deref(), Some("Continue?"));
                count += 1;
                if count == 1 {
                    ReviewDecision::AllowNote
                } else {
                    ReviewDecision::CancelTurn
                }
            },
        )
        .unwrap_err();
        assert_eq!(count, 2);
        assert_eq!(result.to_string(), "Claude turn interrupted by operator");
    }
}
