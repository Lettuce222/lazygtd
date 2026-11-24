use crate::store::{Item, Store};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{
    env,
    ffi::OsString,
    fs::{self, File, Metadata},
    io::{self, BufRead, BufReader, Read, Write},
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::mpsc,
    thread,
    time::{Duration, Instant},
};

type Result<T> = std::result::Result<T, Box<dyn std::error::Error>>;

#[derive(Clone, Copy, Debug, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
enum Provider {
    Codex,
    Claude,
}

impl Provider {
    fn name(self) -> &'static str {
        match self {
            Self::Codex => "codex",
            Self::Claude => "claude",
        }
    }

    fn command(self, executable: &Path, model: Option<&str>, allow_file_reads: bool) -> Command {
        let mut cmd = Command::new(executable);
        match self {
            Self::Codex => cmd.args([
                "-a",
                "never",
                "exec",
                "--json",
                "--ephemeral",
                "--ignore-user-config",
                "--disable",
                "hooks",
                "--disable",
                "plugins",
                "--disable",
                "apps",
                "-c",
                "mcp_servers={}",
                "-c",
                "web_search=\"disabled\"",
                "-c",
                "model_provider=\"openai\"",
                "-c",
                "shell_environment_policy.inherit=\"none\"",
                "-c",
                "shell_environment_policy.ignore_default_excludes=false",
                "-c",
                "project_doc_max_bytes=0",
                "-c",
                "skills.bundled.enabled=false",
                "-c",
                "skills.include_instructions=false",
                "-c",
                "features.skill_mcp_dependency_install=false",
                "-c",
                "features.shell_snapshot=false",
                "--sandbox",
                "read-only",
                "--skip-git-repo-check",
                "--color",
                "never",
            ]),
            Self::Claude => cmd.args([
                "-p",
                "--verbose",
                "--safe-mode",
                "--output-format",
                "stream-json",
                "--permission-mode",
                "default",
                "--setting-sources",
                "",
                "--tools",
                if allow_file_reads {
                    "Read,Glob,Grep"
                } else {
                    ""
                },
                "--strict-mcp-config",
                "--mcp-config",
                "{\"mcpServers\":{}}",
                "--disallowedTools",
                "mcp__*",
                "--no-session-persistence",
            ]),
        };
        if matches!(self, Self::Codex) && !allow_file_reads {
            cmd.args([
                "-c",
                "features.shell_tool=false",
                "-c",
                "features.view_image=false",
            ]);
        }
        if let Some(model) = model {
            cmd.args(["--model", model]);
        }
        if matches!(self, Self::Codex) {
            cmd.arg("-");
        }
        cmd
    }

    fn credential_name(self) -> &'static str {
        match self {
            Self::Codex => "OPENAI_API_KEY",
            Self::Claude => "ANTHROPIC_API_KEY",
        }
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ConfigFile {
    provider: Provider,
    executable: PathBuf,
    workspace: PathBuf,
    model: Option<String>,
    #[serde(default)]
    allow_file_reads: bool,
}

#[derive(Clone)]
pub struct Config {
    provider: Provider,
    executable: PathBuf,
    executable_metadata: Metadata,
    workspace: PathBuf,
    model: Option<String>,
    allow_file_reads: bool,
}

pub const TRUST_WARNING: &str = "実験機能: 公式CLIと信頼できる環境のみ。CLIや管理設定はプレビューにない情報を読み取り、送信できる場合があります。";

fn display_safe(path: &Path) -> bool {
    !path.to_string_lossy().chars().any(|c| {
        c.is_control() || matches!(c, '\u{061c}' | '\u{200b}'..='\u{200f}' | '\u{202a}'..='\u{202e}' | '\u{2060}'..='\u{2069}' | '\u{feff}')
    })
}

fn local_launch_path(path: &Path) -> bool {
    if !path.is_absolute() || !display_safe(path) {
        return false;
    }
    #[cfg(windows)]
    {
        use std::path::{Component, Prefix};
        matches!(path.components().next(), Some(Component::Prefix(prefix))
            if matches!(prefix.kind(), Prefix::Disk(_) | Prefix::VerbatimDisk(_)))
    }
    #[cfg(not(windows))]
    true
}

fn executable_metadata(path: &Path) -> Result<Metadata> {
    if !local_launch_path(path) {
        return Err(
            "agent executable must be an absolute local path without display control characters"
                .into(),
        );
    }
    let metadata = fs::metadata(path)?;
    if !metadata.is_file() {
        return Err("agent executable must be an installed regular file".into());
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = metadata.permissions().mode();
        if mode & 0o111 == 0 || mode & 0o6022 != 0 {
            return Err("agent executable must be executable, non-set-id, and not writable by group or other users".into());
        }
    }
    #[cfg(windows)]
    if !path
        .extension()
        .and_then(|value| value.to_str())
        .is_some_and(|value| value.eq_ignore_ascii_case("exe"))
    {
        return Err("agent executable must be a native .exe, not a shell script or shim".into());
    }
    Ok(metadata)
}

const COMMON_ENV: &[&str] = &[
    "HOME",
    "USERPROFILE",
    "LOCALAPPDATA",
    "APPDATA",
    "PROGRAMDATA",
    "SYSTEMROOT",
    "WINDIR",
    "TMPDIR",
    "TMP",
    "TEMP",
    "XDG_RUNTIME_DIR",
    "LANG",
    "LC_ALL",
    "LC_CTYPE",
];

fn controlled_env(
    provider: Provider,
    get: impl Fn(&str) -> Option<OsString>,
) -> Vec<(&'static str, OsString)> {
    let mut kept = COMMON_ENV
        .iter()
        .copied()
        .chain([provider.credential_name()])
        .filter_map(|name| get(name).map(|value| (name, value)))
        .collect::<Vec<_>>();
    // Never give providers an inherited PATH, proxies, endpoint overrides,
    // shell-startup hooks, dynamic-loader injection, or another service's key.
    #[cfg(unix)]
    {
        kept.push((
            "PATH",
            "/usr/bin:/bin:/usr/sbin:/sbin:/usr/local/bin:/opt/homebrew/bin".into(),
        ));
        kept.push(("SHELL", "/bin/sh".into()));
    }
    #[cfg(windows)]
    if let Some(root) = get("SYSTEMROOT")
        .map(PathBuf::from)
        .filter(|p| local_launch_path(p))
        && let Ok(path) = env::join_paths([root.join("System32"), root.clone()])
    {
        kept.push(("PATH", path));
    }
    kept
}

impl Config {
    pub fn destination(&self) -> String {
        format!(
            "{} / {} / {} / {}",
            self.provider.name(),
            self.model.as_deref().unwrap_or("default model"),
            self.workspace.display(),
            self.executable.display(),
        )
    }

    pub fn allow_file_reads(&self) -> bool {
        self.allow_file_reads
    }

    pub fn load(path: &Path) -> Result<Self> {
        let mut bytes = Vec::new();
        File::open(path)?.take(16385).read_to_end(&mut bytes)?;
        if bytes.len() > 16384 {
            return Err("agent config exceeds 16 KiB".into());
        }
        let config: ConfigFile = serde_json::from_slice(&bytes)?;
        if !local_launch_path(&config.workspace) || !config.workspace.is_dir() {
            return Err("agent workspace must be an existing absolute local directory".into());
        }
        let workspace = config.workspace.canonicalize()?;
        if !local_launch_path(&workspace) {
            return Err("agent workspace resolves to an unsafe or non-local path".into());
        }
        if let Some(model) = &config.model
            && (model.is_empty()
                || model.len() > 120
                || model.starts_with('-')
                || !model
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b"-_.:/@".contains(&b)))
        {
            return Err("invalid model name".into());
        }
        if !local_launch_path(&config.executable) {
            return Err("agent executable must be an absolute path to an official CLI".into());
        }
        let executable = config.executable.canonicalize()?;
        let executable_metadata = executable_metadata(&executable)?;
        let config = Self {
            provider: config.provider,
            executable,
            executable_metadata,
            workspace,
            model: config.model,
            allow_file_reads: config.allow_file_reads,
        };
        config.verify_launch_paths()?;
        Ok(config)
    }

    fn verify_launch_paths(&self) -> Result<()> {
        if self.executable.canonicalize()? != self.executable
            || self.workspace.canonicalize()? != self.workspace
            || !self.workspace.is_dir()
        {
            return Err(
                "agent executable/workspace changed; review the configuration again".into(),
            );
        }
        let metadata = executable_metadata(&self.executable)?;
        let old = &self.executable_metadata;
        let changed = old.len() != metadata.len() || old.modified()? != metadata.modified()?;
        #[cfg(unix)]
        let changed = {
            use std::os::unix::fs::MetadataExt;
            changed
                || old.ino() != metadata.ino()
                || old.dev() != metadata.dev()
                || old.uid() != metadata.uid()
                || old.mode() != metadata.mode()
                || old.ctime() != metadata.ctime()
                || old.ctime_nsec() != metadata.ctime_nsec()
        };
        if changed {
            return Err("agent executable changed; review the configuration again".into());
        }
        // Codex's ignore-user-config does not disable project settings. Reject
        // project config discovery rather than attempt to filter config keys.
        let home_var = if cfg!(windows) { "USERPROFILE" } else { "HOME" };
        let home = env::var_os(home_var).and_then(|path| PathBuf::from(path).canonicalize().ok());
        if self.workspace.parent().is_none()
            || home
                .as_ref()
                .is_some_and(|home| home.starts_with(&self.workspace))
        {
            return Err(
                "choose a specific project directory, not the home directory or filesystem root"
                    .into(),
            );
        }
        if matches!(self.provider, Provider::Codex) {
            for dir in self.workspace.ancestors() {
                // This exact home file is the user config already excluded
                // by --ignore-user-config. No custom CODEX_HOME is inherited.
                if home.as_deref() != Some(dir) {
                    match fs::symlink_metadata(dir.join(".codex").join("config.toml")) {
                        Ok(_) => {
                            return Err("Codex project config is not supported for external dispatch; use a trusted workspace without .codex/config.toml".into());
                        }
                        Err(e) if e.kind() == io::ErrorKind::NotFound => {}
                        Err(e) => return Err(e.into()),
                    }
                }
            }
        }
        Ok(())
    }

    fn command(&self) -> Result<Command> {
        self.verify_launch_paths()?;
        let mut cmd = self.provider.command(
            &self.executable,
            self.model.as_deref(),
            self.allow_file_reads,
        );
        cmd.env_clear()
            .envs(controlled_env(self.provider, |name| env::var_os(name)))
            .current_dir(&self.workspace);
        Ok(cmd)
    }
}

pub fn preview(store: &dyn Store, id: i64, config: &Config) -> Result<()> {
    let item = store.show(id)?;
    println!(
        "{}",
        serde_json::to_string_pretty(&serde_json::json!({
            "provider": config.provider, "workspace": config.workspace, "model": config.model,
            "id": id, "revision": item.revision, "assignee": item.assignee, "prompt": store.agent_prompt(id)?,
            "executable": config.executable, "allow_file_reads": config.allow_file_reads(),
            "mode": "read-only", "warning": TRUST_WARNING
        }))?
    );
    Ok(())
}

pub fn simulate(store: &mut dyn Store, item: &Item) -> Result<()> {
    let run = store.start_run(item.id, item.revision, "demo")?;
    store.progress(run, "reading")?;
    store.finish_run(
        run,
        "review",
        "デモ結果：再現手順を整理しました。内容を確認してから完了にしてください。",
    )?;
    Ok(())
}

pub fn run(store: &mut dyn Store, id: i64, rev: i64, config: &Config) -> Result<()> {
    let result = run_verified(store, id, rev, config, None)?;
    println!("{}", serde_json::to_string(&result)?);
    Ok(())
}

pub fn run_verified(
    store: &mut dyn Store,
    id: i64,
    rev: i64,
    config: &Config,
    expected: Option<&str>,
) -> Result<Option<crate::store::AgentRun>> {
    if let Some(expected) = expected
        && store.agent_prompt(id)? != expected
    {
        return Err("outbound prompt changed; preview again".into());
    }
    let mut cmd = config.command()?;
    let run = store.start_run(id, rev, config.provider.name())?;
    let result = (|| {
        let input = store.run_input(run).map_err(io::Error::other)?;
        if expected.is_some_and(|approved| approved != input) {
            return Err(io::Error::other(
                "frozen outbound prompt differs from preview; not sent",
            ));
        }
        execute(&mut cmd, &input, config.provider, |activity| {
            store.progress(run, activity).map_err(io::Error::other)
        })
    })();
    match result {
        Ok(report) => store.finish_run(run, "review", &report)?,
        Err(error) => {
            store.finish_run(run, "failed", &error.to_string())?;
            return Err(error.into());
        }
    }
    Ok(store.latest_run(id)?)
}

#[derive(Default)]
struct Stream {
    result: Option<String>,
    success: bool,
    error: bool,
}
impl Stream {
    fn accept(&mut self, provider: Provider, v: &Value) -> Option<&'static str> {
        match (provider, v["type"].as_str()?) {
            (Provider::Codex, "thread.started") | (Provider::Claude, "system")
                if matches!(provider, Provider::Codex) || v["subtype"] == "init" =>
            {
                Some("connected")
            }
            (Provider::Codex, "turn.started") => Some("working"),
            (Provider::Codex, "item.started") if v["item"]["type"] == "command_execution" => {
                Some("reading")
            }
            (Provider::Codex, "item.completed") if v["item"]["type"] == "agent_message" => {
                self.result = v["item"]["text"].as_str().map(clean);
                Some("reporting")
            }
            (Provider::Codex, "turn.completed") => {
                self.success = true;
                None
            }
            (Provider::Codex, "turn.failed") => {
                self.error = true;
                None
            }
            (Provider::Claude, "assistant") => {
                let reading = v["message"]["content"]
                    .as_array()
                    .is_some_and(|a| a.iter().any(|x| x["type"] == "tool_use"));
                Some(if reading { "reading" } else { "working" })
            }
            (Provider::Claude, "result") => {
                self.success = v["subtype"] == "success"
                    && v["is_error"] == false
                    && v["permission_denials"].as_array().is_none_or(Vec::is_empty);
                self.error = !self.success;
                self.result = v["result"].as_str().map(clean);
                None
            }
            _ => None,
        }
    }
}

fn clean(s: &str) -> String {
    s.chars()
        .filter(|&c| !c.is_control() || c == '\n' || c == '\t')
        .take(12000)
        .collect()
}

fn execute(
    cmd: &mut Command,
    input: &str,
    provider: Provider,
    progress: impl FnMut(&str) -> io::Result<()>,
) -> io::Result<String> {
    execute_with_timeout(cmd, input, provider, Duration::from_secs(600), progress)
}

fn execute_with_timeout(
    cmd: &mut Command,
    input: &str,
    provider: Provider,
    timeout: Duration,
    mut progress: impl FnMut(&str) -> io::Result<()>,
) -> io::Result<String> {
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
    let mut stdin = child.stdin.take().expect("piped stdin");
    let stdout = child.stdout.take().expect("piped stdout");
    let input = input.to_owned();
    let writer = thread::spawn(move || stdin.write_all(input.as_bytes()));
    let (tx, rx) = mpsc::sync_channel(8);
    thread::spawn(move || {
        let mut reader = BufReader::new(stdout);
        loop {
            // Limit each protocol record; oversized or broken lines fail closed.
            let mut line = Vec::new();
            match (&mut reader).take(1_048_577).read_until(b'\n', &mut line) {
                Ok(0) => break,
                Ok(n) if n <= 1_048_576 => {
                    if tx.send(Ok(line)).is_err() {
                        break;
                    }
                }
                Ok(_) => {
                    let _ = tx.send(Err(io::Error::other("agent event exceeded 1 MiB")));
                    break;
                }
                Err(e) => {
                    let _ = tx.send(Err(e));
                    break;
                }
            }
        }
    });
    let deadline = Instant::now() + timeout;
    let result = (|| {
        let mut stream = Stream::default();
        let mut last = "";
        loop {
            // recv_timeout with a constantly nonempty queue can keep returning
            // data after the timeout. Check wall time before accepting data.
            if Instant::now() >= deadline {
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "agent run timed out",
                ));
            }
            let left = deadline.saturating_duration_since(Instant::now());
            let bytes = match rx.recv_timeout(left) {
                Ok(bytes) => bytes?,
                Err(mpsc::RecvTimeoutError::Disconnected) => break,
                Err(mpsc::RecvTimeoutError::Timeout) => {
                    return Err(io::Error::new(
                        io::ErrorKind::TimedOut,
                        "agent run timed out",
                    ));
                }
            };
            if let Ok(v) = serde_json::from_slice::<Value>(&bytes)
                && let Some(activity) = stream.accept(provider, &v)
                && activity != last
            {
                progress(activity)?;
                last = activity;
            }
        }
        let status = loop {
            if let Some(status) = child.try_wait()? {
                break status;
            }
            if Instant::now() >= deadline {
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "agent run timed out",
                ));
            }
            thread::sleep(Duration::from_millis(50));
        };
        if !status.success() || !stream.success || stream.error {
            return Err(io::Error::other(format!(
                "{} did not return a successful report ({status}); check CLI login/version and permissions",
                provider.name()
            )));
        }
        while !writer.is_finished() {
            if Instant::now() >= deadline {
                return Err(io::Error::other("agent stdin did not close before timeout"));
            }
            thread::sleep(Duration::from_millis(10));
        }
        writer
            .join()
            .map_err(|_| io::Error::other("could not write agent instructions"))??;
        stream
            .result
            .filter(|s| !s.trim().is_empty())
            .ok_or_else(|| io::Error::other("agent returned no report"))
    })();
    if result.is_err() {
        #[cfg(unix)]
        if matches!(child.try_wait(), Ok(None))
            && let Ok(group) = i32::try_from(child.id())
        {
            // The child leads its own process group. Only attempt a group kill
            // while its leader is still unreaped, avoiding stale ID reuse.
            unsafe extern "C" {
                fn killpg(pgrp: i32, sig: i32) -> i32;
            }
            // SAFETY: positive group is from our spawned child; SIGKILL is 9 on Unix.
            unsafe {
                killpg(group, 9);
            }
        }
        let _ = child.kill();
        let _ = child.wait();
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    #[test]
    fn allowlist_drops_endpoint_proxy_loader_and_other_service_overrides() {
        let env = |name: &str| match name {
            "HOME" | "OPENAI_API_KEY" | "ANTHROPIC_API_KEY" | "OPENAI_BASE_URL"
            | "ANTHROPIC_BASE_URL" | "HTTP_PROXY" | "HTTPS_PROXY" | "LD_PRELOAD"
            | "NODE_OPTIONS" | "CODEX_HOME" | "CLAUDE_CONFIG_DIR" | "PATH" => {
                Some(OsString::from("fixture-only"))
            }
            _ => None,
        };
        for provider in [Provider::Codex, Provider::Claude] {
            let values = controlled_env(provider, env);
            assert!(
                values
                    .iter()
                    .any(|(name, _)| *name == provider.credential_name())
            );
            assert!(
                values
                    .iter()
                    .filter(|(_, value)| value == "fixture-only")
                    .all(|(name, _)| *name == "HOME" || *name == provider.credential_name())
            );
        }
    }

    #[test]
    fn provider_defaults_disable_file_tools_without_opt_in() {
        let exe = Path::new("/not-executed");
        let codex = Provider::Codex.command(exe, None, false);
        assert!(
            codex
                .get_args()
                .any(|arg| arg == "features.shell_tool=false")
        );
        let claude = Provider::Claude.command(exe, None, false);
        let args = claude.get_args().collect::<Vec<_>>();
        let tools = args.iter().position(|arg| *arg == "--tools").unwrap();
        assert!(args[tools + 1].is_empty());
        assert!(
            Provider::Claude
                .command(exe, None, true)
                .get_args()
                .any(|arg| arg == "Read,Glob,Grep")
        );
    }

    #[cfg(unix)]
    #[test]
    fn deadline_stops_benign_local_fixture() {
        let started = Instant::now();
        let err = execute_with_timeout(
            Command::new("/bin/sh").args(["-c", "sleep 5"]),
            "synthetic fixture only",
            Provider::Codex,
            Duration::from_millis(100),
            |_| Ok(()),
        )
        .unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::TimedOut);
        assert!(started.elapsed() < Duration::from_secs(3));
    }

    #[test]
    fn normalizes_events_and_rejects_denied_claude_results() {
        let mut a = Stream::default();
        assert_eq!(
            a.accept(
                Provider::Codex,
                &json!({"type":"item.started","item":{"type":"command_execution"}})
            ),
            Some("reading")
        );
        a.accept(
            Provider::Codex,
            &json!({"type":"item.completed","item":{"type":"agent_message","text":"ok\u{1b}"}}),
        );
        a.accept(Provider::Codex, &json!({"type":"turn.completed"}));
        assert!(a.success);
        assert_eq!(a.result.as_deref(), Some("ok"));
        let mut b = Stream::default();
        b.accept(
            Provider::Claude,
            &json!({"type":"result","subtype":"success","is_error":false,
            "result":"report","permission_denials":[{"tool_name":"Bash"}]}),
        );
        assert!(!b.success && b.error);
    }
}
