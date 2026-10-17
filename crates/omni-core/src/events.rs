//! The daemon's live-update bus: one cloneable handle shared by the web
//! server (route events, the `/api/events` stream) and the worker side
//! (job status / stage / progress). Payloads are JSON strings.
//! Sending never blocks and never fails: no subscriber is the normal case.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde_json::{json, Value};
use tokio::sync::broadcast;

/// Receivers lagging further behind than this miss events (the desk then
/// falls back to a full refresh).
const CAPACITY: usize = 256;

/// At most one progress event per job per this long.
const PROGRESS_EVERY: Duration = Duration::from_secs(1);

/// Decides which progress updates become events: the first one of a job, one
/// per second after that, and always 100 %.
#[derive(Debug, Default)]
pub struct ProgressThrottle {
    last: HashMap<i64, Instant>,
}

impl ProgressThrottle {
    pub fn should_emit(&mut self, job_id: i64, progress: f64, now: Instant) -> bool {
        if progress >= 100.0 {
            self.last.remove(&job_id);
            return true;
        }
        match self.last.get(&job_id) {
            Some(prev) if now.saturating_duration_since(*prev) < PROGRESS_EVERY => false,
            _ => {
                self.last.insert(job_id, now);
                true
            }
        }
    }

    /// A stage change or terminal status: the next progress is a "first" again.
    pub fn forget(&mut self, job_id: i64) {
        self.last.remove(&job_id);
    }
}

#[derive(Clone)]
pub struct EventBus {
    tx: broadcast::Sender<String>,
    throttle: Arc<Mutex<ProgressThrottle>>,
}

impl Default for EventBus {
    fn default() -> Self {
        Self::new()
    }
}

impl EventBus {
    pub fn new() -> Self {
        let (tx, _) = broadcast::channel(CAPACITY);
        Self { tx, throttle: Arc::new(Mutex::new(ProgressThrottle::default())) }
    }

    pub fn subscribe(&self) -> broadcast::Receiver<String> {
        self.tx.subscribe()
    }

    fn send(&self, v: Value) {
        let _ = self.tx.send(v.to_string());
    }

    /// `{"type":"<event>"}`, with the job id when the caller knows it.
    pub fn event(&self, event: &str, id: Option<i64>) {
        match id {
            Some(id) => self.send(json!({ "type": event, "id": id })),
            None => self.send(json!({ "type": event })),
        }
    }

    /// A job changed status or stage: always sent. `None` stage or progress
    /// is JSON null: "unchanged / unknown", the desk keeps what it shows.
    pub fn job(&self, id: i64, status: &str, stage: Option<&str>, progress: Option<f64>) {
        if let Ok(mut t) = self.throttle.lock() {
            t.forget(id);
        }
        self.send(json!({ "type": "job", "id": id, "status": status, "stage": stage, "progress": progress }));
    }

    /// A progress tick: sent at most once a second per job (and at 100 %).
    /// `speed` and `eta` ride along with the percentage (the desk card shows them).
    /// `describe` is only called when the event is sent, so the caller can
    /// look the status and stage up lazily.
    pub fn job_progress(&self, id: i64, progress: f64, speed: &str, eta: &str, describe: impl FnOnce() -> Option<(String, Option<String>)>) {
        let go = match self.throttle.lock() {
            Ok(mut t) => t.should_emit(id, progress, Instant::now()),
            Err(_) => false,
        };
        if go {
            if let Some((status, stage)) = describe() {
                self.send(json!({ "type": "job", "id": id, "status": status, "stage": stage, "progress": progress, "speed": speed, "eta": eta }));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(s: String) -> Value {
        serde_json::from_str(&s).expect("payload is JSON")
    }

    #[test]
    fn job_event_is_json_with_id_and_stage() {
        let bus = EventBus::new();
        let mut rx = bus.subscribe();
        bus.job(7, "RUNNING", Some("TRANSCODE"), Some(42.0));
        let v = parse(rx.try_recv().unwrap());
        assert_eq!(v["type"], "job");
        assert_eq!(v["id"], 7);
        assert_eq!(v["status"], "RUNNING");
        assert_eq!(v["stage"], "TRANSCODE");
        assert_eq!(v["progress"], 42.0);
    }

    #[test]
    fn plain_event_is_typed_json_and_id_is_optional() {
        let bus = EventBus::new();
        let mut rx = bus.subscribe();
        bus.event("x", None);
        bus.event("job_updated", Some(3));
        assert_eq!(parse(rx.try_recv().unwrap()), json!({ "type": "x" }));
        assert_eq!(parse(rx.try_recv().unwrap()), json!({ "type": "job_updated", "id": 3 }));
    }

    #[test]
    fn progress_event_carries_speed_and_eta() {
        let bus = EventBus::new();
        let mut rx = bus.subscribe();
        bus.job_progress(8, 33.0, "4 MB/s", "00:20", || Some(("RUNNING".into(), Some("DOWNLOAD".into()))));
        let v = parse(rx.try_recv().unwrap());
        assert_eq!((v["speed"].as_str(), v["eta"].as_str()), (Some("4 MB/s"), Some("00:20")));
        assert_eq!(v["progress"], 33.0);
    }

    #[test]
    fn unknown_stage_and_progress_are_json_null() {
        let bus = EventBus::new();
        let mut rx = bus.subscribe();
        bus.job(4, "FAILED", None, None);
        let v = parse(rx.try_recv().unwrap());
        assert!(v["stage"].is_null());
        assert!(v["progress"].is_null());
        assert_eq!(v["status"], "FAILED");
    }

    #[test]
    fn stage_change_resets_the_progress_throttle() {
        let bus = EventBus::new();
        let mut rx = bus.subscribe();
        let d = || Some(("RUNNING".to_string(), Some("DOWNLOAD".to_string())));
        bus.job_progress(5, 1.0, "1 MB/s", "00:10", d);
        bus.job_progress(5, 2.0, "1 MB/s", "00:10", d); // throttled
        bus.job(5, "RUNNING", Some("TRANSCODE"), None);
        bus.job_progress(5, 3.0, "1 MB/s", "00:10", d); // first of the new stage
        let mut n = 0;
        while rx.try_recv().is_ok() {
            n += 1;
        }
        assert_eq!(n, 3);
    }

    #[test]
    fn no_subscriber_is_not_an_error() {
        let bus = EventBus::new();
        bus.event("x", None);
        bus.job(1, "RUNNING", Some("EXTRACT"), None);
        bus.job_progress(1, 5.0, "1 MB/s", "00:10", || Some(("RUNNING".into(), Some("EXTRACT".into()))));
    }

    #[test]
    fn progress_burst_emits_first_and_final_only() {
        let mut t = ProgressThrottle::default();
        let t0 = Instant::now();
        let mut sent = 0;
        for i in 0..100u64 {
            let p = i as f64;
            if t.should_emit(1, p, t0 + Duration::from_millis(i * 5)) {
                sent += 1;
            }
        }
        assert_eq!(sent, 1, "first only inside one second");
        assert!(t.should_emit(1, 100.0, t0 + Duration::from_millis(600)), "100 % always");
        // another job is independent
        assert!(t.should_emit(2, 1.0, t0));
        // a second later it ticks again
        assert!(t.should_emit(2, 2.0, t0 + Duration::from_millis(1000)));
        assert!(!t.should_emit(2, 3.0, t0 + Duration::from_millis(1500)));
    }

    #[test]
    fn bus_progress_is_throttled_but_lookup_is_lazy() {
        let bus = EventBus::new();
        let mut rx = bus.subscribe();
        for p in [1.0, 2.0, 3.0] {
            bus.job_progress(9, p, "1 MB/s", "00:10", || Some(("RUNNING".into(), Some("DOWNLOAD".into()))));
        }
        bus.job_progress(9, 100.0, "1 MB/s", "00:10", || Some(("RUNNING".into(), Some("DOWNLOAD".into()))));
        let mut n = 0;
        while rx.try_recv().is_ok() {
            n += 1;
        }
        assert_eq!(n, 2);
    }
}
