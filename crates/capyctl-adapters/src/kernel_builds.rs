//! ADR 0014 amendment A11: the kernel builds an engine runs while it starts.
//!
//! A first start with an empty JIT cache compiles kernels (FlashInfer and
//! SGLang's own JIT kernels, PyTorch extensions, TensorFold's CUDA build). On
//! a unified-memory host the compilers' memory is taken from the same pool the
//! startup peak is measured in, so a sample taken during a build measures the
//! build, not the engine. The watcher polls the engine's process group for a
//! compiler and reports when one ran, in the host's clock; the coordinator
//! leaves those samples out of the startup peak.

use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use capyctl_domain::completion::{KernelBuild, ProcessIdentity};

use crate::traits::OwnedProcessLaunch;

/// How often the group is checked for a compiler. A build of several
/// minutes is what matters; a build shorter than a poll may go unseen.
pub const BUILD_POLL: Duration = Duration::from_millis(500);

/// The most spans one start reports; later ones extend the last.
const MAX_BUILDS: usize = 64;

/// What the polls saw. A span opens at the last poll that saw no compiler,
/// so it covers a build from before it was first seen, and closes at the
/// first poll that sees none again.
#[derive(Debug, Default)]
struct Spans {
    last_clear_ms: i64,
    open: Option<i64>,
    done: Vec<KernelBuild>,
}

impl Spans {
    fn observe(&mut self, now_ms: i64, building: bool) {
        if building {
            self.open.get_or_insert(self.last_clear_ms);
        } else {
            if let Some(from) = self.open.take() {
                self.push(from, now_ms);
            }
            self.last_clear_ms = now_ms;
        }
    }

    fn push(&mut self, from_ms: i64, until_ms: i64) {
        let full = self.done.len() >= MAX_BUILDS;
        if let Some(last) = self.done.last_mut() {
            if from_ms <= last.until_ms || full {
                last.until_ms = last.until_ms.max(until_ms);
                return;
            }
        }
        self.done.push(KernelBuild { from_ms, until_ms });
    }

    fn finish(&mut self, now_ms: i64) -> Vec<KernelBuild> {
        if let Some(from) = self.open.take() {
            self.push(from, now_ms);
        }
        std::mem::take(&mut self.done)
    }
}

/// Watches one launch's process group from its spawn until its step ends.
/// Dropping it (a step that failed) stops the polling.
pub struct BuildWatch {
    spans: Arc<Mutex<Spans>>,
    task: tokio::task::JoinHandle<()>,
}

impl BuildWatch {
    /// Start watching the group `api` leads, polling every [`BUILD_POLL`].
    pub fn start(tools: Arc<dyn OwnedProcessLaunch>, api: ProcessIdentity) -> Self {
        Self::every(BUILD_POLL, tools, api)
    }

    /// As [`Self::start`], polling every `poll`.
    pub fn every(poll: Duration, tools: Arc<dyn OwnedProcessLaunch>, api: ProcessIdentity) -> Self {
        let spans = Arc::new(Mutex::new(Spans {
            last_clear_ms: now_ms(),
            ..Spans::default()
        }));
        let shared = spans.clone();
        let task = tokio::spawn(async move {
            loop {
                tokio::time::sleep(poll).await;
                let (tools, api) = (tools.clone(), api.clone());
                // A `/proc` scan, so it runs off the async threads. A failed
                // scan reads as no build: the sample counts, as before A11.
                let building = tokio::task::spawn_blocking(move || tools.building(&api))
                    .await
                    .unwrap_or(false);
                if let Ok(mut spans) = shared.lock() {
                    spans.observe(now_ms(), building);
                }
            }
        });
        Self { spans, task }
    }

    /// Stop watching and report the builds seen. A build still running is
    /// reported up to now.
    pub fn finish(self) -> Vec<KernelBuild> {
        self.task.abort();
        self.spans
            .lock()
            .map(|mut spans| spans.finish(now_ms()))
            .unwrap_or_default()
    }
}

impl Drop for BuildWatch {
    fn drop(&mut self) {
        self.task.abort();
    }
}

/// The host's wall clock in Unix milliseconds, the clock its memory samples
/// carry. A clock before the epoch reads as zero.
fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spans(at: i64) -> Spans {
        Spans {
            last_clear_ms: at,
            ..Spans::default()
        }
    }

    #[test]
    fn a_build_spans_from_the_last_clear_poll_to_the_first_clear_one() {
        let mut s = spans(100);
        s.observe(600, false);
        s.observe(1100, true);
        s.observe(1600, true);
        s.observe(2100, false);
        s.observe(2600, true);
        // The second build begins at the poll that closed the first, so the
        // two touch and merge.
        assert_eq!(
            s.finish(2800),
            [KernelBuild {
                from_ms: 600,
                until_ms: 2800
            }]
        );
        let mut s = spans(0);
        s.observe(500, true);
        s.observe(1000, false);
        s.observe(1500, false);
        s.observe(2000, true);
        assert_eq!(
            s.finish(2200),
            [
                KernelBuild {
                    from_ms: 0,
                    until_ms: 1000
                },
                KernelBuild {
                    from_ms: 1500,
                    until_ms: 2200
                }
            ]
        );
    }

    #[test]
    fn no_build_reports_nothing_and_spans_are_bounded() {
        let mut s = spans(0);
        s.observe(10, false);
        assert!(s.finish(20).is_empty());
        let mut s = spans(0);
        for i in 0..200 {
            let at = 10 * i;
            s.observe(at, i % 3 == 1);
        }
        let done = s.finish(5000);
        assert_eq!(done.len(), MAX_BUILDS);
        assert_eq!(done.last().unwrap().until_ms, 5000);
    }

    struct Building(std::sync::atomic::AtomicBool);
    impl OwnedProcessLaunch for Building {
        fn spawn_durable(
            &self,
            _: &str,
            _: &crate::traits::RenderedCommand,
        ) -> Result<ProcessIdentity, crate::traits::RuntimeError> {
            Err(crate::traits::RuntimeError::Unsupported)
        }
        fn present(&self, _: &ProcessIdentity) -> capyctl_domain::completion::Presence {
            capyctl_domain::completion::Presence::Alive
        }
        fn observe_group(
            &self,
            _: &ProcessIdentity,
        ) -> Result<Vec<ProcessIdentity>, crate::traits::RuntimeError> {
            Ok(Vec::new())
        }
        fn terminate_owned(
            &self,
            _: &[ProcessIdentity],
            _: Duration,
        ) -> Result<(), crate::traits::RuntimeError> {
            Ok(())
        }
        fn building(&self, _: &ProcessIdentity) -> bool {
            self.0.load(std::sync::atomic::Ordering::SeqCst)
        }
    }

    #[tokio::test]
    async fn the_watch_reports_a_build_it_saw_and_nothing_otherwise() {
        let api = ProcessIdentity {
            role: "api".into(),
            pid: 1,
            boot_id: "boot".into(),
            start_ticks: 1,
        };
        let tool = Arc::new(Building(true.into()));
        let before = now_ms();
        let watch = BuildWatch::every(Duration::from_millis(5), tool.clone(), api.clone());
        tokio::time::sleep(Duration::from_millis(50)).await;
        tool.0.store(false, std::sync::atomic::Ordering::SeqCst);
        tokio::time::sleep(Duration::from_millis(50)).await;
        let builds = watch.finish();
        assert_eq!(builds.len(), 1, "{builds:?}");
        assert!(before <= builds[0].from_ms && builds[0].from_ms < builds[0].until_ms);
        let idle = BuildWatch::every(
            Duration::from_millis(5),
            Arc::new(Building(false.into())),
            api,
        );
        tokio::time::sleep(Duration::from_millis(30)).await;
        assert!(idle.finish().is_empty());
    }
}
