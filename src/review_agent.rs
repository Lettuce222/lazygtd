//! Bridge the foreground desk and one live, explicitly opted-in Codex run.
use crate::{
    agent::Config,
    approval_server::{self, ApprovedNote, ReviewDecision},
    store::{AgentRun, Store},
};
use std::{
    io,
    path::Path,
    sync::{Arc, mpsc},
    time::{Duration, Instant},
};
type Result<T> = std::result::Result<T, Box<dyn std::error::Error>>;

#[derive(Clone)]
pub struct Settings {
    pub note: Option<Arc<ApprovedNote>>,
}

impl Settings {
    pub fn new(config: &Config, note: Option<&Path>) -> Result<Self> {
        config.app_server_command()?;
        if config.allow_file_reads() {
            return Err("--codex-review requires allow_file_reads=false; only the approved note is eligible for sharing".into());
        }
        let note = note.map(ApprovedNote::load).transpose()?.map(Arc::new);
        Ok(Self { note })
    }
}

pub struct Notice {
    pub method: String,
    pub deadline: Instant,
    decision: mpsc::SyncSender<ReviewDecision>,
}

impl Notice {
    pub fn decide(self, value: ReviewDecision) -> bool {
        Instant::now() < self.deadline && self.decision.try_send(value).is_ok()
    }
}

pub fn run(
    store: &mut dyn Store,
    id: i64,
    rev: i64,
    config: &Config,
    preview: &str,
    settings: &Settings,
    notices: mpsc::SyncSender<Notice>,
) -> Result<Option<AgentRun>> {
    if store.agent_prompt(id)? != preview {
        return Err("outbound prompt changed; preview again".into());
    }
    let mut command = config.app_server_command()?;
    let run = store.start_run(id, rev, "codex")?;
    let outcome = (|| -> io::Result<String> {
        let input = store.run_input(run).map_err(io::Error::other)?;
        if input != preview {
            return Err(io::Error::other("frozen prompt differs; not sent"));
        }
        approval_server::run_with_review(
            &mut command,
            &input,
            config.workspace(),
            config.model(),
            |activity| store.progress(run, activity).map_err(io::Error::other),
            settings.note.as_deref(),
            |request, deadline| {
                let (decision, rx) = mpsc::sync_channel(1);
                if notices
                    .try_send(Notice {
                        method: request.method.clone(),
                        deadline,
                        decision,
                    })
                    .is_err()
                {
                    return ReviewDecision::Deny;
                }
                rx.recv_timeout(
                    deadline
                        .checked_duration_since(Instant::now())
                        .unwrap_or(Duration::ZERO),
                )
                .unwrap_or(ReviewDecision::Deny)
            },
        )
    })();
    match outcome {
        Ok(report) => store.finish_run(run, "review", &report)?,
        Err(error) => {
            store.finish_run(run, "failed", &error.to_string())?;
            return Err(error.into());
        }
    }
    Ok(store.latest_run(id)?)
}
