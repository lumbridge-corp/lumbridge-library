//! Background work: a tiny in-memory job registry (so long tool calls like "mirror this repo" return
//! a job id immediately) plus helpers the periodic loops in `loops.rs` use.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use uuid::Uuid;

#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum JobState {
    Queued,
    Running,
    Done,
    Failed,
}

#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct Job {
    pub id: Uuid,
    pub kind: String,
    pub target: String,
    pub requested_by: String,
    pub state: JobState,
    pub queued_at: DateTime<Utc>,
    pub started_at: Option<DateTime<Utc>>,
    pub finished_at: Option<DateTime<Utc>>,
    pub detail: Value,
}

const KEEP: usize = 200;

#[derive(Clone, Default)]
pub struct Jobs {
    inner: Arc<Mutex<VecDeque<Job>>>,
}

impl Jobs {
    pub fn queue(&self, kind: &str, target: &str, requested_by: &str) -> Job {
        let job = Job {
            id: Uuid::new_v4(),
            kind: kind.into(),
            target: target.into(),
            requested_by: requested_by.into(),
            state: JobState::Queued,
            queued_at: Utc::now(),
            started_at: None,
            finished_at: None,
            detail: Value::Null,
        };
        let mut q = self.inner.lock().unwrap();
        q.push_front(job.clone());
        q.truncate(KEEP);
        job
    }

    fn update(&self, id: Uuid, f: impl FnOnce(&mut Job)) {
        if let Some(j) = self.inner.lock().unwrap().iter_mut().find(|j| j.id == id) {
            f(j);
        }
    }

    pub fn start(&self, id: Uuid) {
        self.update(id, |j| {
            j.state = JobState::Running;
            j.started_at = Some(Utc::now());
        });
    }

    pub fn finish(&self, id: Uuid, result: anyhow::Result<Value>) {
        self.update(id, |j| {
            j.finished_at = Some(Utc::now());
            match result {
                Ok(v) => {
                    j.state = JobState::Done;
                    j.detail = v;
                }
                Err(e) => {
                    j.state = JobState::Failed;
                    j.detail = serde_json::json!({ "error": format!("{e:#}") });
                }
            }
        });
    }

    pub fn get(&self, id: Uuid) -> Option<Job> {
        self.inner.lock().unwrap().iter().find(|j| j.id == id).cloned()
    }

    pub fn recent(&self, n: usize) -> Vec<Job> {
        self.inner.lock().unwrap().iter().take(n).cloned().collect()
    }

    pub fn active(&self) -> usize {
        self.inner
            .lock()
            .unwrap()
            .iter()
            .filter(|j| matches!(j.state, JobState::Queued | JobState::Running))
            .count()
    }

    /// Is a job of this kind+target already queued or running? (dedupe repeated requests)
    pub fn pending(&self, kind: &str, target: &str) -> Option<Job> {
        self.inner
            .lock()
            .unwrap()
            .iter()
            .find(|j| j.kind == kind && j.target == target && matches!(j.state, JobState::Queued | JobState::Running))
            .cloned()
    }
}
