//! Opt-in, read-only GitHub polling. Store writes happen only on the TUI thread.
use crate::store::{Item, Store, StoreError};
use std::{
    collections::BTreeMap,
    env, fs,
    io::{self, Read},
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc::{self, Receiver, TryRecvError},
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

const INTERVAL: Duration = Duration::from_secs(90);
const MAX_DELAY: Duration = Duration::from_secs(15 * 60);
const HTTP_TIMEOUT: Duration = Duration::from_secs(8);
const BATCH_TIMEOUT: Duration = Duration::from_secs(75);
const MAX_PRS_PER_BATCH: usize = 20;

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
struct Pull {
    owner: String,
    repo: String,
    number: u64,
}

fn parse(url: &str) -> Option<Pull> {
    let path = url.strip_prefix("https://github.com/")?;
    let mut fields = path.strip_suffix('/').unwrap_or(path).split('/');
    let owner = fields.next()?;
    let repo = fields.next()?;
    if fields.next()? != "pull" {
        return None;
    }
    let number = fields.next()?;
    if fields.next().is_some()
        || !(1..=39).contains(&owner.len())
        || owner.starts_with('-')
        || owner.ends_with('-')
        || !owner
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-')
        || !(1..=100).contains(&repo.len())
        || matches!(repo, "." | "..")
        || !repo
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"-_.".contains(&b))
        || !(1..=19).contains(&number.len())
        || number.starts_with('0')
        || !number.bytes().all(|b| b.is_ascii_digit())
    {
        return None;
    }
    Some(Pull {
        owner: owner.to_ascii_lowercase(),
        repo: repo.to_ascii_lowercase(),
        number: number.parse().ok()?,
    })
}

#[derive(Clone, Copy)]
enum Transport {
    Gh,
    Curl,
}

impl Transport {
    fn name(self) -> &'static str {
        match (self, cfg!(windows)) {
            (Self::Gh, false) => "gh",
            (Self::Gh, true) => "gh.exe",
            (Self::Curl, false) => "curl",
            (Self::Curl, true) => "curl.exe",
        }
    }
}

#[derive(Clone)]
struct Client {
    transport: Transport,
    path: PathBuf,
    original: fs::Metadata,
}

fn local_path(path: &Path) -> bool {
    if !path.is_absolute() || path.to_string_lossy().chars().any(char::is_control) {
        return false;
    }
    #[cfg(windows)]
    {
        use std::path::{Component, Prefix};
        matches!(path.components().next(), Some(Component::Prefix(p))
            if matches!(p.kind(), Prefix::Disk(_) | Prefix::VerbatimDisk(_)))
    }
    #[cfg(not(windows))]
    true
}

fn checked_binary(path: &Path, transport: Transport) -> io::Result<fs::Metadata> {
    if !local_path(path) {
        return Err(io::Error::other("watch CLI must be an absolute local path"));
    }
    let name = transport.name();
    let meta = fs::metadata(path)?;
    if !meta.is_file()
        || !path
            .file_name()
            .and_then(|s| s.to_str())
            .is_some_and(|s| s.eq_ignore_ascii_case(name))
    {
        return Err(io::Error::other(
            "choose the installed official watch CLI binary",
        ));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        let mode = meta.mode();
        if mode & 0o111 == 0 || mode & 0o6022 != 0 {
            return Err(io::Error::other(
                "watch CLI must be executable and not set-id or group/world-writable",
            ));
        }
    }
    Ok(meta)
}

impl Client {
    fn new(path: &Path, transport: Transport) -> io::Result<Self> {
        if !local_path(path) {
            return Err(io::Error::other("watch CLI must be an absolute local path"));
        }
        let path = path.canonicalize()?;
        let original = checked_binary(&path, transport)?;
        Ok(Self {
            path,
            original,
            transport,
        })
    }

    fn get(
        &self,
        endpoint: &str,
        filter: &str,
        deadline: Instant,
        cancel: &AtomicBool,
    ) -> Result<String, ()> {
        if self.path.canonicalize().map_err(|_| ())? != self.path {
            return Err(());
        }
        let meta = checked_binary(&self.path, self.transport).map_err(|_| ())?;
        let same = meta.len() == self.original.len()
            && meta.modified().map_err(|_| ())? == self.original.modified().map_err(|_| ())?;
        #[cfg(unix)]
        let same = {
            use std::os::unix::fs::MetadataExt;
            same && meta.ino() == self.original.ino()
                && meta.dev() == self.original.dev()
                && meta.uid() == self.original.uid()
                && meta.mode() == self.original.mode()
                && meta.ctime() == self.original.ctime()
                && meta.ctime_nsec() == self.original.ctime_nsec()
        };
        if !same || cancel.load(Ordering::Relaxed) || Instant::now() >= deadline {
            return Err(());
        }
        // Do not inherit repository context, GH_HOST/GH_CONFIG_DIR, proxies, a
        // custom executable search path, shell startup hooks, or loader options.
        let mut cmd = Command::new(&self.path);
        cmd.env_clear()
            .current_dir(self.path.parent().ok_or(())?)
            .stdin(Stdio::null())
            .stderr(Stdio::null())
            .stdout(Stdio::piped());
        let limit = match self.transport {
            Transport::Gh => {
                cmd.envs(
                    [
                        "HOME",
                        "USERPROFILE",
                        "APPDATA",
                        "LOCALAPPDATA",
                        "SYSTEMROOT",
                        "WINDIR",
                        "XDG_CONFIG_HOME",
                        "XDG_RUNTIME_DIR",
                        "DBUS_SESSION_BUS_ADDRESS",
                        "GH_TOKEN",
                        "GITHUB_TOKEN",
                        "TMPDIR",
                        "TMP",
                        "TEMP",
                    ]
                    .into_iter()
                    .filter_map(|name| env::var_os(name).map(|value| (name, value))),
                )
                .env("GH_PROMPT_DISABLED", "1")
                .env("GH_NO_UPDATE_NOTIFIER", "1")
                .env("GH_TELEMETRY", "false")
                .env("GH_NO_EXTENSION_UPDATE_NOTIFIER", "1")
                .args([
                    "api",
                    "--hostname",
                    "github.com",
                    "--method",
                    "GET",
                    "--jq",
                    filter,
                    endpoint,
                ]);
                128usize
            }
            Transport::Curl => {
                // No curlrc, redirects, proxies, URL expansion, credentials, or
                // endpoint override. Never accept a non-200 response body.
                cmd.envs(
                    ["SYSTEMROOT", "WINDIR"]
                        .into_iter()
                        .filter_map(|name| env::var_os(name).map(|value| (name, value))),
                )
                .args([
                    "-q",
                    "--silent",
                    "--fail",
                    "--proto",
                    "=https",
                    "--noproxy",
                    "*",
                    "--globoff",
                    "--request",
                    "GET",
                    "--max-time",
                    "7",
                    "--max-filesize",
                    "2097152",
                    "--header",
                    "Accept: application/vnd.github+json",
                    "--write-out",
                    "\n%{http_code}",
                    "--url",
                ])
                .arg(format!("https://api.github.com/{endpoint}"));
                2_097_156usize
            }
        };
        #[cfg(unix)]
        cmd.env("PATH", "/usr/bin:/bin:/usr/sbin:/sbin");
        #[cfg(windows)]
        if let Some(root) = env::var_os("SYSTEMROOT")
            .map(PathBuf::from)
            .filter(|p| local_path(p))
            && let Ok(path) = env::join_paths([root.join("System32"), root])
        {
            cmd.env("PATH", path);
        }
        let mut child = cmd.spawn().map_err(|_| ())?;
        let end = deadline.min(Instant::now() + HTTP_TIMEOUT);
        let Some(stdout) = child.stdout.take() else {
            let _ = child.kill();
            let _ = child.wait();
            return Err(());
        };
        // Drain this bounded output concurrently instead of waiting on a
        // potentially full pipe. Never let this reader hold up cancellation.
        let (output_tx, output_rx) = mpsc::channel();
        if thread::Builder::new()
            .name("github-pr-output".into())
            .spawn(move || {
                let mut bytes = Vec::with_capacity(128);
                let result = stdout.take((limit + 1) as u64).read_to_end(&mut bytes);
                let _ = output_tx.send(result.map(|_| bytes));
            })
            .is_err()
        {
            let _ = child.kill();
            let _ = child.wait();
            return Err(());
        }
        loop {
            match child.try_wait() {
                Ok(Some(status)) if status.success() => {
                    while !cancel.load(Ordering::Relaxed) && Instant::now() < end {
                        match output_rx.recv_timeout(Duration::from_millis(25)) {
                            Ok(Ok(bytes)) if bytes.len() <= limit => {
                                let str = String::from_utf8(bytes).map_err(|_| ())?;
                                return match self.transport {
                                    Transport::Gh => Ok(str),
                                    Transport::Curl => curl_scalar(&str, filter),
                                };
                            }
                            Err(mpsc::RecvTimeoutError::Timeout) => {}
                            _ => return Err(()),
                        }
                    }
                    return Err(());
                }
                Ok(None) if Instant::now() < end && !cancel.load(Ordering::Relaxed) => {
                    thread::sleep(Duration::from_millis(25))
                }
                _ => {
                    let _ = child.kill();
                    let _ = child.wait();
                    return Err(());
                }
            }
        }
    }
}

const REPO_FILTER: &str = ".visibility";
const PULL_FILTER: &str = r#"if .base.repo.private == false then .state else "unavailable" end"#;

fn curl_scalar(output: &str, filter: &str) -> Result<String, ()> {
    let (json, status) = output.rsplit_once('\n').ok_or(())?;
    if status != "200" {
        return Err(());
    }
    let json: serde_json::Value = serde_json::from_str(json).map_err(|_| ())?;
    match filter {
        REPO_FILTER
            if json["private"].as_bool() == Some(false)
                && json["visibility"].as_str() == Some("public") =>
        {
            Ok("public".into())
        }
        PULL_FILTER if json["base"]["repo"]["private"].as_bool() == Some(false) => {
            match json["state"].as_str() {
                Some(state @ ("open" | "closed")) => Ok(state.into()),
                _ => Err(()),
            }
        }
        _ => Err(()),
    }
}

type Repo = (String, String);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum State {
    Closed,
    Open,
    Unavailable,
}

fn pull_status(
    pull: &Pull,
    public: &mut BTreeMap<Repo, bool>,
    mut get: impl FnMut(&str, &str) -> Result<String, ()>,
) -> State {
    let repo = (pull.owner.clone(), pull.repo.clone());
    let endpoint = format!("repos/{}/{}", pull.owner, pull.repo);
    let visible = *public.entry(repo).or_insert_with(
        || matches!(get(&endpoint, REPO_FILTER).as_deref(), Ok(s) if s.trim() == "public"),
    );
    if !visible {
        return State::Unavailable;
    }
    let Ok(result) = get(&format!("{endpoint}/pulls/{}", pull.number), PULL_FILTER) else {
        return State::Unavailable;
    };
    // Recheck visibility in the PR response in case it changed mid-poll.
    match result.trim() {
        "closed" => State::Closed,
        "open" => State::Open,
        _ => State::Unavailable,
    }
}

struct Target {
    id: i64,
    rev: i64,
    urls: Vec<(String, Pull)>,
}

impl Target {
    fn from_item(item: Item) -> Option<Self> {
        if item.done || item.box_id != "waiting" {
            return None;
        }
        let urls: Vec<_> = item
            .planning
            .watch?
            .split_whitespace()
            .map(|url| Some((url.to_owned(), parse(url)?)))
            .collect::<Option<_>>()?;
        // Never silently skip a non-PR URL, since all watched URLs must close.
        if urls.is_empty() || urls.len() > MAX_PRS_PER_BATCH {
            return None;
        }
        Some(Self {
            id: item.id,
            rev: item.revision,
            urls,
        })
    }
}

struct Resolved {
    id: i64,
    rev: i64,
    urls: Vec<String>,
}

#[derive(Default)]
struct Batch {
    resolved: Vec<Resolved>,
    unavailable: bool,
}

fn sweep(targets: Vec<Target>, mut query: impl FnMut(&Pull) -> State) -> Batch {
    let mut result = Batch::default();
    let mut cached = BTreeMap::new();
    for target in targets {
        let mut all_closed = true;
        for (_, pull) in &target.urls {
            match *cached.entry(pull.clone()).or_insert_with(|| query(pull)) {
                State::Closed => {}
                State::Open => {
                    all_closed = false;
                    break;
                }
                State::Unavailable => {
                    all_closed = false;
                    result.unavailable = true;
                    break;
                }
            }
        }
        if all_closed {
            result.resolved.push(Resolved {
                id: target.id,
                rev: target.rev,
                urls: target.urls.into_iter().map(|(url, _)| url).collect(),
            });
        }
    }
    result
}

fn take_batch(mut targets: Vec<Target>, after: i64) -> Vec<Target> {
    targets.sort_unstable_by_key(|target| target.id);
    let start = targets.partition_point(|target| target.id <= after);
    targets.rotate_left(start);
    let mut budget = MAX_PRS_PER_BATCH;
    targets
        .into_iter()
        .take_while(|target| {
            if target.urls.len() > budget {
                return false;
            }
            budget -= target.urls.len();
            true
        })
        .collect()
}

fn apply_results(store: &mut dyn Store, resolved: Vec<Resolved>) -> (usize, bool) {
    let mut moved = 0;
    let mut failed = false;
    for entry in resolved {
        let urls: Vec<_> = entry.urls.iter().map(String::as_str).collect();
        match store.watch_resolve(entry.id, entry.rev, &urls, "system:watch") {
            Ok(item) if item.box_id == "next" => moved += 1,
            Ok(_) | Err(StoreError::Stale { .. } | StoreError::Missing(_)) => {}
            Err(_) => failed = true,
        }
    }
    (moved, failed)
}

struct Worker {
    reply: Receiver<Batch>,
    cancel: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl Drop for Worker {
    fn drop(&mut self) {
        self.cancel.store(true, Ordering::Relaxed);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

pub struct Watcher {
    client: Client,
    next: Option<Instant>,
    delay: Duration,
    after: i64,
    worker: Option<Worker>,
}

impl Watcher {
    pub fn new(path: &Path) -> io::Result<Self> {
        Self::with_transport(path, Transport::Gh)
    }

    pub fn new_curl(path: &Path) -> io::Result<Self> {
        Self::with_transport(path, Transport::Curl)
    }

    fn with_transport(path: &Path, transport: Transport) -> io::Result<Self> {
        Ok(Self {
            client: Client::new(path, transport)?,
            next: None,
            delay: INTERVAL,
            after: 0,
            worker: None,
        })
    }

    pub fn tick(&mut self, store: &mut dyn Store, now: Instant) -> io::Result<usize> {
        if let Some(worker) = &self.worker {
            let batch = match worker.reply.try_recv() {
                Ok(batch) => batch,
                Err(TryRecvError::Empty) => return Ok(0),
                Err(TryRecvError::Disconnected) => {
                    self.worker = None;
                    self.next = Some(now + INTERVAL);
                    return Err(io::Error::other("GitHub PR watch worker stopped"));
                }
            };
            self.worker = None;
            let first_failure = batch.unavailable && self.delay == INTERVAL;
            self.delay = if batch.unavailable {
                (self.delay * 2).min(MAX_DELAY)
            } else {
                INTERVAL
            };
            self.next = Some(now + self.delay);
            let (moved, failed) = apply_results(store, batch.resolved);
            if moved == 0 && (first_failure || failed) {
                return Err(io::Error::other(
                    "GitHub PR watch could not confirm some items; left them in waiting and will retry",
                ));
            }
            return Ok(moved);
        }
        if self.next.is_some_and(|next| now < next) {
            return Ok(0);
        }
        self.next = Some(now + self.delay);
        let targets = store
            .list()
            .map_err(io::Error::other)?
            .into_iter()
            .filter_map(Target::from_item)
            .collect();
        let targets = take_batch(targets, self.after);
        if let Some(last) = targets.last() {
            self.after = last.id;
        } else {
            return Ok(0);
        }
        let client = self.client.clone();
        let (tx, rx) = mpsc::channel();
        let cancel = Arc::new(AtomicBool::new(false));
        let signal = Arc::clone(&cancel);
        let thread = thread::Builder::new()
            .name("github-pr-watch".into())
            .spawn(move || {
                let deadline = Instant::now() + BATCH_TIMEOUT;
                let mut public = BTreeMap::new();
                let result = sweep(targets, |pull| {
                    pull_status(pull, &mut public, |endpoint, filter| {
                        client.get(endpoint, filter, deadline, &signal)
                    })
                });
                let _ = tx.send(result);
            })?;
        self.worker = Some(Worker {
            reply: rx,
            cancel,
            thread: Some(thread),
        });
        Ok(0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{planning::PlanField, store::Sqlite};

    const A: &str = "https://github.com/Owner/Repo/pull/1";
    const B: &str = "https://github.com/owner/repo/pull/2/";
    const BAD: &str = "https://other.example/owner/repo/pull/2";

    fn item(store: &mut Sqlite, watch: &str) -> Item {
        let row = store.add_with_body("watched", "", "user").unwrap();
        let row = store
            .move_to(row.id, row.revision, "waiting", "user")
            .unwrap();
        store
            .change_meta(row.id, row.revision, PlanField::Watch, watch, "user")
            .unwrap()
    }

    #[cfg(unix)]
    #[test]
    fn watcher_uses_only_fixed_gets_and_applies_in_background() {
        use std::os::unix::fs::PermissionsExt;
        struct Temp(PathBuf);
        impl Drop for Temp {
            fn drop(&mut self) {
                let _ = fs::remove_dir_all(&self.0);
            }
        }
        let dir = Temp(env::temp_dir().join(format!(
            "lazygtd-pr-watch-{}-{}",
            std::process::id(),
            std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH)
                .unwrap().as_nanos()
        )));
        fs::create_dir(&dir.0).unwrap();
        let exe = dir.0.join("gh");
        fs::write(
            &exe,
            r#"#!/bin/sh
set -eu
test "$#" -eq 8
test "$1 $2 $3 $4 $5 $6" = "api --hostname github.com --method GET --jq"
test "$GH_PROMPT_DISABLED" = "1"
test "$GH_TELEMETRY" = "false"
case "$8" in
  repos/owner/repo) test "$7" = ".visibility"; printf 'public\n' ;;
  repos/owner/repo/pulls/1) printf 'closed\n' ;;
  repos/owner/repo/pulls/2) exit 1 ;;
  *) exit 2 ;;
esac
"#,
        )
        .unwrap();
        fs::set_permissions(&exe, fs::Permissions::from_mode(0o700)).unwrap();
        let mut watcher = Watcher::new(&exe).unwrap();
        let mut store = Sqlite::demo().unwrap();
        let resolved = item(&mut store, A);
        let unavailable = item(&mut store, B);
        let now = Instant::now();
        assert_eq!(watcher.tick(&mut store, now).unwrap(), 0);
        let deadline = now + Duration::from_secs(10);
        loop {
            if watcher.tick(&mut store, now).unwrap() != 0 {
                break;
            }
            assert!(Instant::now() < deadline);
            thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(store.show(resolved.id).unwrap().box_id, "next");
        assert_eq!(store.show(unavailable.id).unwrap().box_id, "waiting");
        assert_eq!(watcher.next, Some(now + INTERVAL * 2));
    }

    #[test]
    fn curl_rejects_non_200_invalid_private_or_unrecognized_results() {
        let public = r#"{"visibility":"public","private":false}"#;
        let closed = r#"{"state":"closed","base":{"repo":{"private":false}}}"#;
        assert_eq!(
            curl_scalar(&format!("{public}\n200"), REPO_FILTER).unwrap(),
            "public"
        );
        assert_eq!(
            curl_scalar(&format!("{closed}\n200"), PULL_FILTER).unwrap(),
            "closed"
        );
        for code in [
            "000", "201", "301", "302", "307", "401", "403", "404", "429",
        ] {
            assert!(curl_scalar(&format!("{closed}\n{code}"), PULL_FILTER).is_err());
        }
        for json in [
            "{}",
            "invalid json",
            r#"{"state":"closed","base":{"repo":{"private":true}}}"#,
            r#"{"state":"closed","base":{"repo":{"private":"false"}}}"#,
            r#"{"state":"merged","base":{"repo":{"private":false}}}"#,
        ] {
            assert!(curl_scalar(&format!("{json}\n200"), PULL_FILTER).is_err());
        }
        for json in [
            r#"{"visibility":"internal","private":false}"#,
            r#"{"visibility":"public","private":true}"#,
            r#"{"visibility":"public","private":null}"#,
        ] {
            assert!(curl_scalar(&format!("{json}\n200"), REPO_FILTER).is_err());
        }
    }

    #[test]
    fn endpoint_contract_is_strict() {
        let parsed = parse(A).unwrap();
        assert_eq!(
            (&*parsed.owner, &*parsed.repo, parsed.number),
            ("owner", "repo", 1)
        );
        assert!(parse(B).is_some());
        for url in [
            BAD,
            "http://github.com/o/r/pull/1",
            "https://github.com@bad.test/o/r/pull/1",
            "https://github.com:443/o/r/pull/1",
            "https://github.com/o/%2e%2e/pull/1",
            "https://github.com/o/../pull/1",
            "https://github.com/o/r/issues/1",
            "https://github.com/o/r/pull/1?q=2",
            "https://github.com/o/r/pull/1#comment",
            "https://github.com/o/r/pull/01",
            "https://github.com/o/r/pull/1/files",
        ] {
            assert!(parse(url).is_none(), "{url}");
        }
    }

    #[test]
    fn asks_for_number_only_after_verifying_public_repo() {
        let pull = parse(A).unwrap();
        for visibility in [Ok("private"), Ok("internal"), Ok(""), Err(())] {
            let mut public = BTreeMap::new();
            let mut calls = Vec::new();
            assert_eq!(
                pull_status(&pull, &mut public, |endpoint, filter| {
                    calls.push((endpoint.to_owned(), filter.to_owned()));
                    visibility.map(str::to_owned)
                }),
                State::Unavailable
            );
            assert_eq!(calls, [("repos/owner/repo".into(), ".visibility".into())]);
        }
        let mut public = BTreeMap::new();
        let mut lookup = |endpoint: &str, _: &str| -> Result<String, ()> {
            match endpoint {
                "repos/owner/repo" => Ok("public\n".into()),
                "repos/owner/repo/pulls/1" => Ok("closed".into()),
                "repos/owner/repo/pulls/2" => Ok("unavailable".into()),
                _ => unreachable!(),
            }
        };
        assert_eq!(pull_status(&pull, &mut public, &mut lookup), State::Closed);
        assert_eq!(
            pull_status(&parse(B).unwrap(), &mut public, &mut lookup),
            State::Unavailable
        );
    }

    #[test]
    fn only_all_closed_with_same_revision_moves_the_item() {
        let mut store = Sqlite::demo().unwrap();
        let row = item(&mut store, &format!("{A} {B}"));
        let invalid = item(&mut store, &format!("{A} {BAD}"));
        assert!(Target::from_item(invalid.clone()).is_none());
        for nonclosure in [State::Open, State::Unavailable] {
            let batch = sweep(vec![Target::from_item(row.clone()).unwrap()], |pull| {
                if pull.number == 1 {
                    State::Closed
                } else {
                    nonclosure
                }
            });
            assert!(batch.resolved.is_empty());
            assert_eq!(batch.unavailable, nonclosure == State::Unavailable);
            assert_eq!(store.show(row.id).unwrap().box_id, "waiting");
        }
        let stale = sweep(vec![Target::from_item(row.clone()).unwrap()], |_| {
            State::Closed
        });
        let row = store
            .edit_body(row.id, row.revision, "touched during poll", "user")
            .unwrap();
        assert_eq!(apply_results(&mut store, stale.resolved), (0, false));
        let fresh = sweep(vec![Target::from_item(row.clone()).unwrap()], |_| {
            State::Closed
        });
        assert_eq!(apply_results(&mut store, fresh.resolved), (1, false));
        assert_eq!(store.show(row.id).unwrap().box_id, "next");
        assert_eq!(store.show(invalid.id).unwrap().box_id, "waiting");
    }

    #[test]
    fn bounded_batches_rotate_and_deduplicate() {
        let mut store = Sqlite::demo().unwrap();
        let many = std::iter::repeat_n(A, MAX_PRS_PER_BATCH)
            .collect::<Vec<_>>()
            .join(" ");
        let first = item(&mut store, &many);
        let second = item(&mut store, B);
        let rows = || {
            [first.clone(), second.clone()]
                .into_iter()
                .filter_map(Target::from_item)
                .collect()
        };
        let batch = take_batch(rows(), 0);
        assert_eq!(batch.len(), 1);
        assert_eq!(batch[0].id, first.id);
        let mut calls = 0;
        sweep(batch, |_| {
            calls += 1;
            State::Closed
        });
        assert_eq!(calls, 1);
        let batch = take_batch(rows(), first.id);
        assert_eq!(batch.len(), 1);
        assert_eq!(batch[0].id, second.id);
    }
}
