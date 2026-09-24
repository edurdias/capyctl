//! Owned local host-pressure sampling for the F2 qualification runner.
//!
//! This observes `/proc/meminfo` only. It grants no engine authority and does not
//! close server admission by itself. The runner must consult a fresh handle
//! status before each submission and compose its run-abort API on a breach.
use crate::f2_pressure::{PressureAbort, PressureBounds, PressureGuard, PressureStatus};
use mllm_agent::memory::{read_host_memory, HostMemorySample};
use std::{
    sync::{Arc, Mutex},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};
use tokio::sync::watch;

const PERIOD: Duration = Duration::from_millis(250);
const MAX_AGE: Duration = Duration::from_secs(2);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MonitorStatus {
    Warming,
    Ready,
    Aborted(PressureAbort),
    Stopped,
}

struct State {
    guard: PressureGuard,
    status: MonitorStatus,
    last_monotonic: Instant,
    last_wall: Option<i64>,
}
struct Shared {
    started: Instant,
    state: Mutex<State>,
}

/// A current diagnostic pressure gate, not a cached pass or a resource grant.
#[derive(Clone)]
pub struct PressureHandle(Arc<Shared>);
impl PressureHandle {
    /// Rechecks age and clock validity even when the sampler has stalled. Abort
    /// latches for this monitor's lifetime; no later sample can reopen it.
    pub fn status(&self) -> MonitorStatus {
        self.0.status()
    }
    pub fn bounds(&self) -> Option<PressureBounds> {
        self.0.state.lock().ok().and_then(|s| s.guard.bounds())
    }
}

type Reader = Arc<dyn Fn() -> Result<HostMemorySample, PressureAbort> + Send + Sync>;

/// Owns one sampler task and at most one blocking proc read. Dropping it closes
/// every retained handle immediately and stops future reads. A running read is
/// not cancelled; shutdown joins it. No unbounded retry queue or sample history.
pub struct PressureMonitor {
    shared: Arc<Shared>,
    stop: watch::Sender<bool>,
    task: Option<tokio::task::JoinHandle<()>>,
}
impl PressureMonitor {
    /// Requires a Tokio runtime. The only production collector is the bounded
    /// local proc reader; no caller path or remote host label selects a source.
    pub fn start_local() -> Self {
        Self::with_reader(Arc::new(|| {
            read_host_memory().map_err(|_| PressureAbort::Observation)
        }))
    }
    fn with_reader(reader: Reader) -> Self {
        let started = Instant::now();
        let shared = Arc::new(Shared {
            started,
            state: Mutex::new(State {
                guard: PressureGuard::default(),
                status: MonitorStatus::Warming,
                last_monotonic: started,
                last_wall: None,
            }),
        });
        let (stop, receiver) = watch::channel(false);
        let task = tokio::spawn(run(shared.clone(), reader, receiver));
        Self {
            shared,
            stop,
            task: Some(task),
        }
    }
    pub fn handle(&self) -> PressureHandle {
        PressureHandle(self.shared.clone())
    }
    pub async fn shutdown(mut self) {
        self.shared.close();
        self.stop.send_replace(true);
        if let Some(task) = self.task.take() {
            let _ = task.await;
        }
    }
}
impl Drop for PressureMonitor {
    fn drop(&mut self) {
        self.shared.close();
        self.stop.send_replace(true);
    }
}

fn now_ms() -> Result<i64, PressureAbort> {
    i64::try_from(
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(|_| PressureAbort::Observation)?
            .as_millis(),
    )
    .map_err(|_| PressureAbort::Observation)
}
impl Shared {
    fn close(&self) {
        if let Ok(mut state) = self.state.lock() {
            if !matches!(state.status, MonitorStatus::Aborted(_)) {
                state.status = MonitorStatus::Stopped;
            }
        }
    }
    fn abort(&self, reason: PressureAbort) {
        if let Ok(mut state) = self.state.lock() {
            if matches!(state.status, MonitorStatus::Warming | MonitorStatus::Ready) {
                state.status = MonitorStatus::Aborted(reason);
            }
        }
    }
    fn status(&self) -> MonitorStatus {
        let Ok(mut state) = self.state.lock() else {
            return MonitorStatus::Aborted(PressureAbort::Observation);
        };
        if !matches!(state.status, MonitorStatus::Warming | MonitorStatus::Ready) {
            return state.status;
        }
        let result = (|| {
            let now = now_ms()?;
            if state.last_wall.is_none() {
                return if self.started.elapsed() < MAX_AGE {
                    Ok(PressureStatus::Warming)
                } else {
                    Err(PressureAbort::Observation)
                };
            }
            let monotonic = u64::try_from(self.started.elapsed().as_millis())
                .map_err(|_| PressureAbort::Observation)?;
            state.guard.status_at(monotonic, now)
        })();
        state.status = match result {
            Ok(PressureStatus::Warming) => MonitorStatus::Warming,
            Ok(PressureStatus::Ready) => MonitorStatus::Ready,
            Err(reason) => MonitorStatus::Aborted(reason),
        };
        state.status
    }
    fn remaining(&self) -> Duration {
        let Ok(state) = self.state.lock() else {
            return Duration::ZERO;
        };
        let mut age = state.last_monotonic.elapsed();
        if let Some(sampled) = state.last_wall {
            let Ok(now) = now_ms() else {
                return Duration::ZERO;
            };
            let Some(wall_age) = now.checked_sub(sampled).and_then(|v| u64::try_from(v).ok())
            else {
                return Duration::ZERO;
            };
            age = age.max(Duration::from_millis(wall_age));
        }
        MAX_AGE.saturating_sub(age)
    }
    fn observe(&self, sample: HostMemorySample) {
        let Ok(mut state) = self.state.lock() else {
            return;
        };
        if !matches!(state.status, MonitorStatus::Warming | MonitorStatus::Ready) {
            return;
        }
        let result = now_ms().and_then(|now| {
            let monotonic = u64::try_from(self.started.elapsed().as_millis())
                .map_err(|_| PressureAbort::Observation)?;
            state.guard.observe(&sample, monotonic, now)
        });
        state.status = match result {
            Ok(PressureStatus::Warming) => MonitorStatus::Warming,
            Ok(PressureStatus::Ready) => MonitorStatus::Ready,
            Err(reason) => MonitorStatus::Aborted(reason),
        };
        state.last_monotonic = Instant::now();
        state.last_wall = Some(sample.memory.sampled_at_ms);
    }
}

async fn run(shared: Arc<Shared>, reader: Reader, mut stop: watch::Receiver<bool>) {
    let mut ticks = tokio::time::interval(PERIOD);
    ticks.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        tokio::select! {
            biased;
            _ = stop.changed() => return,
            _ = ticks.tick() => {},
        }
        if !matches!(
            shared.status(),
            MonitorStatus::Warming | MonitorStatus::Ready
        ) {
            return;
        }
        let source = reader.clone();
        let mut read = tokio::task::spawn_blocking(move || source());
        tokio::select! {
            biased;
            _ = stop.changed() => { let _ = read.await; return; },
            _ = tokio::time::sleep(shared.remaining()) => {
                shared.abort(PressureAbort::Observation);
                // Retain the single read until it exits. A late result cannot
                // update the latched state or trigger another read.
                let _ = read.await;
                return;
            },
            result = &mut read => match result {
                Ok(Ok(sample)) => shared.observe(sample),
                _ => { shared.abort(PressureAbort::Observation); return; },
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use mllm_agent::memory::parse_meminfo;
    use std::sync::{
        atomic::{AtomicUsize, Ordering},
        Arc, Condvar, Mutex,
    };
    use std::time::Duration;

    #[tokio::test]
    async fn unsafe_sample_latches_abort_and_stops_collection() {
        let calls = Arc::new(AtomicUsize::new(0));
        let reader_calls = calls.clone();
        let monitor = PressureMonitor::with_reader(Arc::new(move || {
            reader_calls.fetch_add(1, Ordering::SeqCst);
            parse_meminfo("MemTotal: 134217728 kB\nMemAvailable: 1048576 kB\nSwapTotal: 0 kB\nSwapFree: 0 kB\n", now_ms()?)
                .map_err(|_| PressureAbort::Observation)
        }));
        let handle = monitor.handle();
        tokio::time::timeout(Duration::from_secs(1), async {
            while handle.status() == MonitorStatus::Warming {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        assert_eq!(
            handle.status(),
            MonitorStatus::Aborted(PressureAbort::Headroom)
        );
        monitor.shutdown().await;
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        assert_eq!(
            handle.status(),
            MonitorStatus::Aborted(PressureAbort::Headroom)
        );
    }

    struct Gate(Mutex<bool>, Condvar);
    impl Gate {
        fn release(&self) {
            *self.0.lock().unwrap() = true;
            self.1.notify_all();
        }
    }
    struct Release(Arc<Gate>);
    impl Drop for Release {
        fn drop(&mut self) {
            self.0.release();
        }
    }

    #[tokio::test]
    async fn stalled_read_aborts_before_return_and_shutdown_joins_the_only_reader() {
        let gate = Arc::new(Gate(Mutex::new(false), Condvar::new()));
        let _release = Release(gate.clone());
        let reader_gate = gate.clone();
        let calls = Arc::new(AtomicUsize::new(0));
        let reader_calls = calls.clone();
        let monitor = PressureMonitor::with_reader(Arc::new(move || {
            reader_calls.fetch_add(1, Ordering::SeqCst);
            let _guard = reader_gate
                .1
                .wait_timeout_while(
                    reader_gate.0.lock().unwrap(),
                    Duration::from_secs(10),
                    |released| !*released,
                )
                .unwrap();
            // Even a fresh, safe late result cannot reopen a timed-out monitor.
            parse_meminfo("MemTotal: 134217728 kB\nMemAvailable: 104857600 kB\nSwapTotal: 0 kB\nSwapFree: 0 kB\n", now_ms()?)
                .map_err(|_| PressureAbort::Observation)
        }));
        let handle = monitor.handle();
        tokio::time::timeout(Duration::from_secs(4), async {
            // Observe published state without calling status(), which itself
            // enforces freshness. This proves the task's independent watchdog.
            while monitor.shared.state.lock().unwrap().status == MonitorStatus::Warming {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
        assert_eq!(
            handle.status(),
            MonitorStatus::Aborted(PressureAbort::Observation)
        );
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        let join = tokio::spawn(monitor.shutdown());
        tokio::time::sleep(Duration::from_millis(20)).await;
        assert!(!join.is_finished());
        gate.release();
        tokio::time::timeout(Duration::from_secs(1), join)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        assert_eq!(
            handle.status(),
            MonitorStatus::Aborted(PressureAbort::Observation)
        );
    }

    #[tokio::test]
    async fn dropped_monitor_closes_even_a_retained_handle() {
        let monitor = PressureMonitor::with_reader(Arc::new(|| Err(PressureAbort::Observation)));
        let handle = monitor.handle();
        assert_eq!(handle.status(), MonitorStatus::Warming);
        drop(monitor);
        assert_eq!(handle.status(), MonitorStatus::Stopped);
    }

    #[tokio::test]
    async fn successful_collection_stays_in_baseline_and_stops_after_join() {
        let calls = Arc::new(AtomicUsize::new(0));
        let reader_calls = calls.clone();
        let monitor = PressureMonitor::with_reader(Arc::new(move || {
            reader_calls.fetch_add(1, Ordering::SeqCst);
            parse_meminfo("MemTotal: 134217728 kB\nMemAvailable: 104857600 kB\nSwapTotal: 0 kB\nSwapFree: 0 kB\n", now_ms()?)
                .map_err(|_| PressureAbort::Observation)
        }));
        let handle = monitor.handle();
        tokio::time::timeout(Duration::from_secs(2), async {
            while calls.load(Ordering::SeqCst) < 3 {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
        assert_eq!(handle.status(), MonitorStatus::Warming);
        assert_eq!(handle.bounds().unwrap().managed_bytes, 96_i64 << 30);
        monitor.shutdown().await;
        assert_eq!(handle.status(), MonitorStatus::Stopped);
        let stopped_calls = calls.load(Ordering::SeqCst);
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert_eq!(calls.load(Ordering::SeqCst), stopped_calls);
    }

    #[tokio::test]
    async fn panicked_collector_aborts_instead_of_retrying() {
        let calls = Arc::new(AtomicUsize::new(0));
        let reader_calls = calls.clone();
        let monitor = PressureMonitor::with_reader(Arc::new(move || {
            reader_calls.fetch_add(1, Ordering::SeqCst);
            panic!("test-only collector panic");
        }));
        let handle = monitor.handle();
        tokio::time::timeout(Duration::from_secs(1), async {
            while handle.status() == MonitorStatus::Warming {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        assert_eq!(
            handle.status(),
            MonitorStatus::Aborted(PressureAbort::Observation)
        );
        monitor.shutdown().await;
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }
}
