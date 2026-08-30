use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crate::task::ExtIds;

#[derive(Debug, Clone)]
pub struct DispatchJob {
    pub task: String,
    pub node: String,
    pub model: String,
    pub prompt: String,
    pub cwd: std::path::PathBuf,
    pub attempt: u32,
    pub title: String,
}

#[derive(Debug, Clone)]
pub enum HarnessSignal {
    Started,
    Succeeded { summary: String },
    Failed { summary: String },
    Interrupted,
}

#[derive(Debug, Clone)]
pub enum AdapterEvent {
    DispatchAccepted {
        task: String,
        ext: ExtIds,
    },
    DispatchFailed {
        task: String,
        definitive: bool,
        detail: String,
    },
    Signal {
        task: String,
        signal: HarnessSignal,
    },
    Unknown {
        task: String,
        detail: String,
    },
    Notice {
        detail: String,
    },
}

pub trait HarnessAdapter: Send {
    fn name(&self) -> &'static str;
    fn check(&mut self) -> anyhow::Result<()>;
    fn dispatch(&mut self, job: DispatchJob);
    fn cancel(&mut self, task: &str);
    fn active(&self) -> usize;
    fn idle_for(&self) -> Duration;
    fn touch(&mut self);
    fn shutdown(&mut self);
}

#[derive(Default)]
pub struct SharedClock {
    last_activity: Option<Mutex<Instant>>,
    active: AtomicUsize,
    running: AtomicBool,
}

impl SharedClock {
    pub fn new() -> Self {
        SharedClock {
            last_activity: Some(Mutex::new(Instant::now())),
            active: AtomicUsize::new(0),
            running: AtomicBool::new(false),
        }
    }

    pub fn touch_now(&self) {
        if let Some(clock) = &self.last_activity {
            *clock.lock().unwrap() = Instant::now();
        }
    }

    pub fn idle_for(&self) -> Duration {
        if !self.is_running() {
            return Duration::ZERO;
        }
        match &self.last_activity {
            Some(clock) => clock.lock().unwrap().elapsed(),
            None => Duration::ZERO,
        }
    }

    pub fn active_count(&self) -> usize {
        self.active.load(Ordering::SeqCst)
    }

    pub fn set_active(&self, n: usize) {
        self.active.store(n, Ordering::SeqCst);
    }

    pub fn set_running(&self, on: bool) {
        self.running.store(on, Ordering::SeqCst);
    }

    pub fn is_running(&self) -> bool {
        self.running.load(Ordering::SeqCst)
    }
}

struct FakeState {
    jobs: Mutex<Vec<DispatchJob>>,
    cancels: Mutex<Vec<String>>,
    script: Mutex<std::collections::VecDeque<Vec<AdapterEvent>>>,
    check_error: Mutex<Option<String>>,
    shutdowns: AtomicUsize,
    clock: SharedClock,
}

pub struct FakeAdapter {
    name: &'static str,
    events: Sender<AdapterEvent>,
    state: Arc<FakeState>,
}

#[derive(Clone)]
pub struct FakeHandle {
    pub events: Sender<AdapterEvent>,
    state: Arc<FakeState>,
    name: &'static str,
}

pub fn fake_adapter(
    name: &'static str,
    events: Sender<AdapterEvent>,
) -> (Box<dyn HarnessAdapter>, FakeHandle) {
    let state = Arc::new(FakeState {
        jobs: Mutex::new(Vec::new()),
        cancels: Mutex::new(Vec::new()),
        script: Mutex::new(std::collections::VecDeque::new()),
        check_error: Mutex::new(None),
        shutdowns: AtomicUsize::new(0),
        clock: SharedClock::new(),
    });
    (
        Box::new(FakeAdapter {
            name,
            events: events.clone(),
            state: state.clone(),
        }),
        FakeHandle {
            events,
            state,
            name,
        },
    )
}

impl FakeHandle {
    pub fn jobs(&self) -> Vec<DispatchJob> {
        self.state.jobs.lock().unwrap().clone()
    }

    pub fn job(&self, task: &str) -> Option<DispatchJob> {
        self.jobs().into_iter().find(|j| j.task == task)
    }

    pub fn cancels(&self) -> Vec<String> {
        self.state.cancels.lock().unwrap().clone()
    }

    pub fn shutdowns(&self) -> u32 {
        self.state.shutdowns.load(Ordering::SeqCst) as u32
    }

    pub fn set_check_error(&self, err: Option<String>) {
        *self.state.check_error.lock().unwrap() = err;
    }

    pub fn script_next(&self, events: Vec<AdapterEvent>) {
        self.state.script.lock().unwrap().push_back(events);
    }

    pub fn accept(&self, task: &str) {
        let _ = self.events.send(AdapterEvent::DispatchAccepted {
            task: task.to_owned(),
            ext: ExtIds::default(),
        });
    }

    pub fn signal(&self, task: &str, signal: HarnessSignal) {
        let _ = self.events.send(AdapterEvent::Signal {
            task: task.to_owned(),
            signal,
        });
    }

    pub fn name(&self) -> &'static str {
        self.name
    }

    pub fn finish(&self, task: &str) {
        self.state
            .clock
            .set_active(self.state.clock.active_count().saturating_sub(1));
        self.signal(
            task,
            HarnessSignal::Succeeded {
                summary: String::new(),
            },
        );
    }
}

impl HarnessAdapter for FakeAdapter {
    fn name(&self) -> &'static str {
        self.name
    }

    fn check(&mut self) -> anyhow::Result<()> {
        match self.state.check_error.lock().unwrap().clone() {
            Some(err) => anyhow::bail!("{err}"),
            None => Ok(()),
        }
    }

    fn dispatch(&mut self, job: DispatchJob) {
        self.state.jobs.lock().unwrap().push(job.clone());
        let clock = &self.state.clock;
        clock.set_active(clock.active_count() + 1);
        clock.set_running(true);
        clock.touch_now();
        let scripted = self.state.script.lock().unwrap().pop_front();
        match scripted {
            Some(events) => {
                for ev in events {
                    let _ = self.events.send(ev);
                }
            }
            None => {
                let _ = self.events.send(AdapterEvent::DispatchAccepted {
                    task: job.task.clone(),
                    ext: ExtIds::default(),
                });
            }
        }
    }

    fn cancel(&mut self, task: &str) {
        self.state.cancels.lock().unwrap().push(task.to_owned());
    }

    fn active(&self) -> usize {
        self.state.clock.active_count()
    }

    fn idle_for(&self) -> Duration {
        self.state.clock.idle_for()
    }

    fn touch(&mut self) {
        self.state.clock.touch_now();
    }

    fn shutdown(&mut self) {
        self.state.clock.set_active(0);
        self.state.clock.set_running(false);
        self.state.shutdowns.fetch_add(1, Ordering::SeqCst);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fake_dispatch_emits_acceptance_and_tracks_job() {
        let (tx, rx) = std::sync::mpsc::channel();
        let (mut adapter, handle) = fake_adapter("codex", tx);
        adapter.check().unwrap();
        adapter.dispatch(DispatchJob {
            task: "t1".into(),
            node: "refiner".into(),
            model: "m".into(),
            prompt: "p".into(),
            cwd: ".".into(),
            attempt: 1,
            title: "T".into(),
        });
        match rx.recv_timeout(Duration::from_secs(1)).unwrap() {
            AdapterEvent::DispatchAccepted { task, .. } => assert_eq!(task, "t1"),
            other => panic!("unexpected {other:?}"),
        }
        assert_eq!(handle.jobs().len(), 1);
    }

    #[test]
    fn stopped_clock_is_not_idle() {
        let clock = SharedClock::new();
        assert_eq!(clock.idle_for(), Duration::ZERO);
        clock.set_running(true);
        std::thread::sleep(Duration::from_millis(1));
        assert!(clock.idle_for() > Duration::ZERO);
        clock.set_running(false);
        assert_eq!(clock.idle_for(), Duration::ZERO);
    }
}
