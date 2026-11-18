//! Long work (fetching tokens, installing tools, building) runs on a thread of
//! its own and writes lines as it goes; the page asks for what is new.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use serde::Serialize;

#[derive(Clone, Serialize)]
pub struct Snapshot {
    pub id: u64,
    pub title: String,
    pub lines: Vec<String>,
    /// How many lines came before `lines`.
    pub from: usize,
    /// `running`, `done` or `failed`.
    pub state: &'static str,
    pub error: Option<String>,
    pub result: Option<serde_json::Value>,
}

struct Job {
    title: String,
    lines: Vec<String>,
    state: &'static str,
    error: Option<String>,
    result: Option<serde_json::Value>,
}

#[derive(Default, Clone)]
pub struct Jobs {
    inner: Arc<Mutex<(u64, BTreeMap<u64, Job>)>>,
}

/// What a job writes its lines with.
#[derive(Clone)]
pub struct Say {
    jobs: Jobs,
    id: u64,
}

impl Say {
    pub fn line(&self, text: impl Into<String>) {
        if let Ok(mut g) = self.jobs.inner.lock() {
            if let Some(j) = g.1.get_mut(&self.id) {
                j.lines.push(text.into());
            }
        }
    }
}

impl Jobs {
    /// Start `work` on a thread; its id, for [`Jobs::get`].
    pub fn spawn(&self, title: &str, work: impl FnOnce(&Say) -> Result<serde_json::Value, String> + Send + 'static) -> u64 {
        let id = {
            let mut g = self.inner.lock().expect("the job list");
            g.0 += 1;
            let id = g.0;
            g.1.insert(id, Job { title: title.into(), lines: Vec::new(), state: "running", error: None, result: None });
            // Keep the last fifty.
            while g.1.len() > 50 {
                let first = *g.1.keys().next().expect("not empty");
                g.1.remove(&first);
            }
            id
        };
        let say = Say { jobs: self.clone(), id };
        std::thread::spawn(move || {
            let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| work(&say)))
                .unwrap_or_else(|_| Err("the helper hit a bug here (it panicked); its terminal says more".into()));
            if let Ok(mut g) = say.jobs.inner.lock() {
                if let Some(j) = g.1.get_mut(&id) {
                    match outcome {
                        Ok(v) => {
                            j.state = "done";
                            j.result = Some(v);
                        }
                        Err(e) => {
                            j.state = "failed";
                            j.error = Some(e);
                        }
                    }
                }
            }
        });
        id
    }

    /// The job's lines from `since` on, and how it stands.
    pub fn get(&self, id: u64, since: usize) -> Option<Snapshot> {
        let g = self.inner.lock().ok()?;
        let j = g.1.get(&id)?;
        let from = since.min(j.lines.len());
        Some(Snapshot { id, title: j.title.clone(), lines: j.lines[from..].to_vec(), from, state: j.state, error: j.error.clone(), result: j.result.clone() })
    }

    /// Whether a job with this title is still running.
    pub fn running(&self, title: &str) -> bool {
        self.inner.lock().map(|g| g.1.values().any(|j| j.title == title && j.state == "running")).unwrap_or(false)
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn a_job_reports_its_lines_and_end() {
        let jobs = super::Jobs::default();
        let id = jobs.spawn("t", |say| {
            say.line("one");
            say.line("two");
            Ok(serde_json::json!({"ok": true}))
        });
        let mut snap = jobs.get(id, 0).unwrap();
        for _ in 0..200 {
            if snap.state != "running" {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
            snap = jobs.get(id, 0).unwrap();
        }
        assert_eq!((snap.state, snap.lines.len()), ("done", 2));
        assert_eq!(jobs.get(id, 1).unwrap().lines, vec!["two".to_string()]);
        let failed = jobs.spawn("f", |_| Err("no".into()));
        let mut snap = jobs.get(failed, 0).unwrap();
        for _ in 0..200 {
            if snap.state != "running" {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
            snap = jobs.get(failed, 0).unwrap();
        }
        assert_eq!((snap.state, snap.error.as_deref()), ("failed", Some("no")));
    }
}
