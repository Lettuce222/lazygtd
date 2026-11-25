use crate::{
    sessions,
    store::{Item, Store},
};
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

    fn command(
        self,
        executable: &Path,
        model: Option<&str>,
        allow_file_reads: bool,
        workspace: &Path,
        session_mode: SessionMode,
        resume: Option<&str>,
    ) -> Command {
        let mut cmd = Command::new(executable);
        match self {
            Self::Codex => cmd.args([
                "-a",
                "never",
                "exec",
                "--json",
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
            // The resume command otherwise inherits the previous workspace.
            cmd.arg("-C").arg(workspace);
            if session_mode == SessionMode::Ephemeral {
                cmd.arg("--ephemeral");
            }
            if let Some(id) = resume {
                cmd.args(["resume", id]);
            }
            cmd.arg("-");
        } else {
            if session_mode == SessionMode::Ephemeral {
                cmd.arg("--no-session-persistence");
            }
            if let Some(id) = resume {
                cmd.args(["--resume", id]);
            }
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

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
enum SessionMode {
    #[default]
    Ephemeral,
    Persist,
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
    #[serde(default)]
    session_mode: SessionMode,
    resume_session: Option<String>,
}

#[derive(Clone)]
pub struct Config {
    provider: Provider,
    executable: PathBuf,
    executable_metadata: Metadata,
    workspace: PathBuf,
    model: Option<String>,
    allow_file_reads: bool,
    session_mode: SessionMode,
    resume_session: Option<String>,
}

const SESSION_WARNING: &str = "保存型: CLIの既定プロフィールに履歴を保存します。再開時は、今回の入力以外に以前の会話やツール出力も送信対象になり得ます。";
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
            "{} / {} / {} / {}{}",
            self.provider.name(),
            self.model.as_deref().unwrap_or("default model"),
            self.workspace.display(),
            self.executable.display(),
            self.resume_session
                .as_ref()
                .map(|id| format!(" / resume {id}"))
                .unwrap_or_else(|| if self.session_mode == SessionMode::Persist {
                    " / new persistent session".into()
                } else {
                    String::new()
                }),
        )
    }

    pub fn allow_file_reads(&self) -> bool {
        self.allow_file_reads
    }

    pub fn history_warning(&self) -> Option<&'static str> {
        (self.session_mode == SessionMode::Persist).then_some(SESSION_WARNING)
    }

    fn session_request(&self) -> Result<Option<sessions::Request<'_>>> {
        if self.session_mode == SessionMode::Ephemeral {
            return Ok(None);
        }
        Ok(Some(sessions::Request {
            workspace: self
                .workspace
                .to_str()
                .ok_or("native session workspace must be valid Unicode")?,
            expected: self.resume_session.as_deref(),
        }))
    }

    pub fn load(path: &Path) -> Result<Self> {
        let mut bytes = Vec::new();
        File::open(path)?.take(16385).read_to_end(&mut bytes)?;
        if bytes.len() > 16384 {
            return Err("agent config exceeds 16 KiB".into());
        }
        let config: ConfigFile = serde_json::from_slice(&bytes)?;
        if let Some(id) = &config.resume_session {
            if config.session_mode != SessionMode::Persist {
                return Err("resume_session requires session_mode: persist".into());
            }
            if !sessions::valid_id(id) {
                return Err("resume_session must be a canonical lowercase UUID".into());
            }
        }
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
            session_mode: config.session_mode,
            resume_session: config.resume_session,
        };
        config.session_request()?;
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

    pub(crate) fn workspace(&self) -> &Path {
        &self.workspace
    }

    pub(crate) fn model(&self) -> Option<&str> {
        self.model.as_deref()
    }

    pub(crate) fn review_command(&self, selected: &str) -> Result<Command> {
        if self.provider.name() != selected {
            return Err("review option must match the configured provider".into());
        }
        if self.session_mode != SessionMode::Ephemeral || self.allow_file_reads {
            return Err(
                "live review requires an ephemeral configuration with file tools disabled".into(),
            );
        }
        self.verify_launch_paths()?;
        let mut cmd = Command::new(&self.executable);
        cmd.env_clear()
            .envs(controlled_env(self.provider, |name| env::var_os(name)))
            .current_dir(&self.workspace);
        let key = self.provider.credential_name();
        if !cmd
            .get_envs()
            .any(|(name, value)| name == key && value.is_some_and(|value| !value.is_empty()))
        {
            return Err(format!("live review requires {key} in the launch environment").into());
        }
        Ok(cmd)
    }

    fn command(&self) -> Result<Command> {
        self.verify_launch_paths()?;
        let mut cmd = self.provider.command(
            &self.executable,
            self.model.as_deref(),
            self.allow_file_reads,
            &self.workspace,
            self.session_mode,
            self.resume_session.as_deref(),
        );
        // A persisted conversation uses the native CLI’s existing profile,
        // not a possibly unrelated API key inherited from the calling shell.
        // No custom profile directory is inherited in either mode.
        cmd.env_clear()
            .envs(controlled_env(self.provider, |name| {
                (self.session_mode == SessionMode::Ephemeral
                    || name != self.provider.credential_name())
                .then(|| env::var_os(name))
                .flatten()
            }))
            .current_dir(&self.workspace);
        Ok(cmd)
    }
}

pub fn preview(store: &dyn Store, id: i64, config: &Config) -> Result<()> {
    let item = store.show(id)?;
    if let Some(scope) = config.session_request()? {
        store.check_session(id, config.provider.name(), scope)?;
    }
    println!(
        "{}",
        serde_json::to_string_pretty(&serde_json::json!({
            "provider": config.provider, "workspace": config.workspace, "model": config.model,
            "id": id, "revision": item.revision, "assignee": item.assignee, "prompt": store.agent_prompt(id)?,
            "executable": config.executable, "allow_file_reads": config.allow_file_reads(),
            "mode": "read-only", "warning": TRUST_WARNING,
            "session_mode": config.session_mode, "resume_session": config.resume_session,
            "history_warning": config.history_warning()
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
    let scope = config.session_request()?;
    if let Some(scope) = scope {
        store.check_session(id, config.provider.name(), scope)?;
    }
    let mut cmd = config.command()?;
    let run = store.start_run_scoped(id, rev, config.provider.name(), scope)?;
    let result = (|| {
        let input = store.run_input(run).map_err(io::Error::other)?;
        if expected.is_some_and(|approved| approved != input) {
            return Err(io::Error::other(
                "frozen outbound prompt differs from preview; not sent",
            ));
        }
        execute(&mut cmd, &input, config.provider, scope, |activity| {
            store.progress(run, activity).map_err(io::Error::other)
        })
    })();
    match result {
        Ok(output) => {
            if let Err(e) =
                store.finish_run_scoped(run, "review", &output.report, output.session_id.as_deref())
            {
                // A failed binding must not leave the task locked in running.
                store.finish_run(
                    run,
                    "failed",
                    "provider native session could not be associated; no automatic retry",
                )?;
                return Err(e.into());
            }
        }
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
    session_id: Option<String>,
    inconsistent_session: bool,
}
impl Stream {
    fn accept(&mut self, provider: Provider, v: &Value) -> Option<&'static str> {
        match (provider, v["type"].as_str()?) {
            (Provider::Codex, "thread.started") | (Provider::Claude, "system")
                if matches!(provider, Provider::Codex) || v["subtype"] == "init" =>
            {
                match provider {
                    Provider::Codex => self.observe_session(v["thread_id"].as_str()),
                    Provider::Claude => self.observe_session(v["session_id"].as_str()),
                }
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
                self.observe_session(v["session_id"].as_str());
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

    fn observe_session(&mut self, id: Option<&str>) {
        if let Some(id) = id {
            if !sessions::valid_id(id)
                || self.session_id.as_deref().is_some_and(|first| first != id)
            {
                self.inconsistent_session = true;
            } else {
                self.session_id = Some(id.to_owned());
            }
        }
    }

    fn confirmed_session(
        &self,
        scope: Option<sessions::Request<'_>>,
    ) -> io::Result<Option<String>> {
        let Some(request) = scope else {
            return Ok(None);
        };
        let id = self.session_id.as_deref();
        if self.inconsistent_session
            || id.is_none()
            || request
                .expected
                .is_some_and(|expected| id != Some(expected))
        {
            return Err(io::Error::other(
                "provider did not confirm the exact native session; no automatic retry",
            ));
        }
        Ok(self.session_id.clone())
    }
}

#[derive(Debug)]
struct ExecutionOutput {
    report: String,
    session_id: Option<String>,
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
    scope: Option<sessions::Request<'_>>,
    progress: impl FnMut(&str) -> io::Result<()>,
) -> io::Result<ExecutionOutput> {
    execute_with_timeout(
        cmd,
        input,
        provider,
        scope,
        Duration::from_secs(600),
        progress,
    )
}

fn execute_with_timeout(
    cmd: &mut Command,
    input: &str,
    provider: Provider,
    scope: Option<sessions::Request<'_>>,
    timeout: Duration,
    mut progress: impl FnMut(&str) -> io::Result<()>,
) -> io::Result<ExecutionOutput> {
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
        let session_id = stream.confirmed_session(scope)?;
        let report = stream
            .result
            .filter(|s| !s.trim().is_empty())
            .ok_or_else(|| io::Error::other("agent returned no report"))?;
        Ok(ExecutionOutput { report, session_id })
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
        let codex = Provider::Codex.command(
            exe,
            None,
            false,
            Path::new("/fixture"),
            SessionMode::Ephemeral,
            None,
        );
        assert!(
            codex
                .get_args()
                .any(|arg| arg == "features.shell_tool=false")
        );
        let claude = Provider::Claude.command(
            exe,
            None,
            false,
            Path::new("/fixture"),
            SessionMode::Ephemeral,
            None,
        );
        let args = claude.get_args().collect::<Vec<_>>();
        let tools = args.iter().position(|arg| *arg == "--tools").unwrap();
        assert!(args[tools + 1].is_empty());
        assert!(
            Provider::Claude
                .command(
                    exe,
                    None,
                    true,
                    Path::new("/fixture"),
                    SessionMode::Ephemeral,
                    None
                )
                .get_args()
                .any(|arg| arg == "Read,Glob,Grep")
        );
    }

    #[cfg(unix)]
    #[test]
    fn confirmed_dispatch_resumes_only_the_bound_provider_uuid() {
        use crate::store::Sqlite;
        use std::os::unix::fs::PermissionsExt;
        let root = std::env::temp_dir().join(format!(
            "lazygtd-native-fixture-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir(&root).unwrap();
        struct Cleanup(PathBuf);
        impl Drop for Cleanup {
            fn drop(&mut self) {
                let _ = fs::remove_dir_all(&self.0);
            }
        }
        let _cleanup = Cleanup(root.clone());
        let native = "01892af6-8270-7ced-86c1-929b50dadcb5";
        let wrong = "361bf867-7c96-4097-9b3f-01cc00f12629";
        for (provider, resume_key, events) in [
            (
                "codex",
                "resume",
                format!(
                    "{{\"type\":\"thread.started\",\"thread_id\":\"{native}\"}}\n{{\"type\":\"item.completed\",\"item\":{{\"type\":\"agent_message\",\"text\":\"fixture\"}}}}\n{{\"type\":\"turn.completed\"}}"
                ),
            ),
            (
                "claude",
                "--resume",
                format!(
                    "{{\"type\":\"system\",\"subtype\":\"init\",\"session_id\":\"{native}\"}}\n{{\"type\":\"result\",\"subtype\":\"success\",\"is_error\":false,\"result\":\"fixture\",\"session_id\":\"{native}\"}}"
                ),
            ),
        ] {
            let workspace = root.join(provider);
            fs::create_dir(&workspace).unwrap();
            let exe = root.join(format!("{provider}-fixture"));
            fs::write(
                &exe,
                format!(
                    "#!/bin/sh\nset -eu\n[ -z \"${{OPENAI_API_KEY:-}}\" ] && [ -z \"${{ANTHROPIC_API_KEY:-}}\" ]\ncase \" $* \" in *' --last '*|*' --continue '*|*' --ephemeral '*|*' --no-session-persistence '*) exit 2;; esac\ncase \" $* \" in *' {resume_key} '*) case \" $* \" in *' {resume_key} {native} '*|*' {resume_key} {wrong} '*) ;; *) exit 3;; esac ;; esac\ncat >/dev/null\ncat <<'EOF'\n{events}\nEOF\n"
                )
            ).unwrap();
            fs::set_permissions(&exe, fs::Permissions::from_mode(0o700)).unwrap();
            let config_path = root.join("agent.json");
            let mut cfg = json!({
                "provider": provider, "executable": exe, "workspace": workspace,
                "session_mode": "persist"
            });
            fs::write(&config_path, cfg.to_string()).unwrap();
            let mut db = Sqlite::open(&root.join(format!("{provider}.db"))).unwrap();
            let item = db.add_with_body("synthetic", "", "user").unwrap();
            let item = db
                .send_message(item.id, item.revision, "first task-only prompt")
                .unwrap();
            let result = run_verified(
                &mut db,
                item.id,
                item.revision,
                &Config::load(&config_path).unwrap(),
                None,
            )
            .unwrap()
            .unwrap();
            assert_eq!(result.session_id.as_deref(), Some(native));

            let item = db.show(item.id).unwrap();
            let item = db
                .send_message(item.id, item.revision, "next task-only prompt")
                .unwrap();
            cfg["resume_session"] = json!(native);
            fs::write(&config_path, cfg.to_string()).unwrap();
            let config = Config::load(&config_path).unwrap();
            assert!(config.history_warning().is_some());
            assert!(config.destination().contains(native));
            let command = config.command().unwrap();
            let args = command.get_args().collect::<Vec<_>>();
            assert!(args.windows(2).any(|args| args == [resume_key, native]));
            assert!(!args.iter().any(|arg| matches!(
                arg.to_str(),
                Some("--last" | "--continue" | "--ephemeral" | "--no-session-persistence")
            )));
            assert_eq!(
                run_verified(&mut db, item.id, item.revision, &config, None)
                    .unwrap()
                    .unwrap()
                    .session_id
                    .as_deref(),
                Some(native)
            );

            let second = db.add_with_body("unrelated", "", "user").unwrap();
            let second = db
                .send_message(second.id, second.revision, "reject before spawn")
                .unwrap();
            assert!(
                run_verified(&mut db, second.id, second.revision, &config, None)
                    .unwrap_err()
                    .to_string()
                    .contains("another task or workspace")
            );
            assert!(db.latest_run(second.id).unwrap().is_none());

            cfg["resume_session"] = json!(wrong);
            fs::write(&config_path, cfg.to_string()).unwrap();
            let item = db.show(item.id).unwrap();
            let item = db
                .send_message(item.id, item.revision, "mismatched result must fail")
                .unwrap();
            let err = run_verified(
                &mut db,
                item.id,
                item.revision,
                &Config::load(&config_path).unwrap(),
                None,
            )
            .unwrap_err();
            assert!(err.to_string().contains("exact native session"));
            let run = db.latest_run(item.id).unwrap().unwrap();
            assert_eq!(run.state, "failed");
            assert_eq!(run.session_id, None);
        }
    }

    #[cfg(unix)]
    #[test]
    fn deadline_stops_benign_local_fixture() {
        let started = Instant::now();
        let err = execute_with_timeout(
            Command::new("/bin/sh").args(["-c", "sleep 5"]),
            "synthetic fixture only",
            Provider::Codex,
            None,
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
