//! Bridge the foreground desk and one live, explicitly opted-in provider run.
use crate::{
    agent::Config,
    approval_server::{self, ApprovedNote, Review, ReviewDecision},
    claude_review,
    store::{AgentRun, Store},
};
use std::{
    io,
    path::Path,
    sync::{Arc, mpsc},
    time::{Duration, Instant},
};
type Result<T> = std::result::Result<T, Box<dyn std::error::Error>>;

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Provider {
    Codex,
    Claude,
}
impl Provider {
    fn key(self) -> &'static str {
        match self {
            Self::Codex => "codex",
            Self::Claude => "claude",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Codex => "Codex",
            Self::Claude => "Claude",
        }
    }
}

#[derive(Clone)]
pub struct Settings {
    pub provider: Provider,
    pub note: Option<Arc<ApprovedNote>>,
}

impl Settings {
    pub fn new(config: &Config, provider: Provider, note: Option<&Path>) -> Result<Self> {
        config.review_command(provider.key())?;
        if note.is_some() && provider != Provider::Codex {
            return Err("--approved-note is only available for Codex review".into());
        }
        let note = note.map(ApprovedNote::load).transpose()?.map(Arc::new);
        Ok(Self { provider, note })
    }
}

pub struct Notice {
    pub item_id: i64,
    pub provider: &'static str,
    pub method: String,
    pub detail: Option<String>,
    pub deadline: Instant,
    decision: mpsc::SyncSender<ReviewDecision>,
}

impl Notice {
    #[cfg(test)]
    pub(crate) fn test_claude(item_id: i64) -> (Self, mpsc::Receiver<ReviewDecision>) {
        let (decision, rx) = mpsc::sync_channel(1);
        (
            Self {
                item_id,
                provider: "Claude",
                method: "AskUserQuestion".into(),
                detail: None,
                deadline: Instant::now() + Duration::from_secs(30),
                decision,
            },
            rx,
        )
    }

    pub fn badge(&self) -> &'static str {
        match self.provider {
            "Claude" => "Claude 承認待ち · ",
            _ => "Codex 承認待ち · ",
        }
    }

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
    let mut command = config.review_command(settings.provider.key())?;
    let run = store.start_run(id, rev, settings.provider.key())?;
    let outcome = (|| -> io::Result<String> {
        let input = store.run_input(run).map_err(io::Error::other)?;
        if input != preview {
            return Err(io::Error::other("frozen prompt differs; not sent"));
        }
        let status = |activity: &str| store.progress(run, activity).map_err(io::Error::other);
        let review = |request: &Review, deadline: Instant| {
            let (decision, rx) = mpsc::sync_channel(1);
            if notices
                .try_send(Notice {
                    item_id: id,
                    provider: settings.provider.label(),
                    method: request.method.clone(),
                    detail: request.detail.clone(),
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
        };
        match settings.provider {
            Provider::Codex => approval_server::run_with_review(
                &mut command,
                &input,
                config.workspace(),
                config.model(),
                status,
                settings.note.as_deref(),
                review,
            ),
            Provider::Claude => claude_review::run_with_review(
                &mut command,
                &input,
                config.workspace(),
                config.model(),
                status,
                review,
            ),
        }
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
