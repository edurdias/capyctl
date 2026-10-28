//! ADR 0037 §4, §6: the activation trace in the controller.
//!
//! An activation (SPEC §10 steps 2–7) serves every request that joined it, so
//! it is its own trace, rooted at `capyctl.activation`. A request-driven
//! activation's root is the router's ([`crate::LifecyclePort::activate_for_request_traced`]);
//! the authority binds the deployment to it while the activation runs, so the
//! switch (drain, release) and the worker (reserve, launch or wake,
//! readiness, canary) hang their steps under it. An activation nothing bound
//! (an operator's start or wake) gets its own root from the worker; an
//! operator's switch (`start --evict`) gets one from the switcher.
//!
//! ADR 0037 §6: a step's work runs [`within`] the step, and every lifecycle
//! command it sends a host that declared `trace_context` carries the step's
//! `traceparent` in the command envelope ([`current_step`]), never in the
//! command itself.
//!
//! Everything here is memory only: a restart loses it, and work resumed
//! after one is not traced.

use std::collections::HashMap;
use std::future::Future;
use std::sync::{Arc, Mutex, RwLock};

use capyctl_trace::{ActivationKind, Attr, Name, Outcome, Parent, Span, SpanCx, Tracing};

tokio::task_local! {
    static STEP: SpanCx;
}

/// Run `work` as part of the step whose context is `step`: the lifecycle
/// commands it sends carry that context (ADR 0037 §6). Without one (tracing
/// off, or no activation), `work` runs as it is.
pub async fn within<F: Future>(step: Option<SpanCx>, work: F) -> F::Output {
    match step {
        Some(cx) => STEP.scope(cx, work).await,
        None => work.await,
    }
}

/// One activation step. It ends `failed` unless closed otherwise, so a step
/// left by an early return or an error is never shown as completed.
#[derive(Debug, Default)]
pub struct Step {
    span: Option<Span>,
}

impl Step {
    pub fn new(span: Span) -> Self {
        Self { span: Some(span) }
    }

    pub fn cx(&self) -> Option<SpanCx> {
        self.span.as_ref()?.cx()
    }

    pub fn set(&mut self, attr: Attr) {
        if let Some(span) = &mut self.span {
            span.set(attr);
        }
    }

    /// The step completed.
    pub fn completed(self) {
        self.end(Outcome::Completed);
    }

    /// The step ended with `outcome`.
    pub fn end(mut self, outcome: Outcome) {
        if let Some(mut span) = self.span.take() {
            span.set(Attr::Outcome(outcome));
            span.end();
        }
    }

    /// The step failed with a closed CapyCTL code (ADR 0037 §5: never an
    /// error's text).
    pub fn fail(mut self, code: &str) {
        if let Some(mut span) = self.span.take() {
            span.set(Attr::Outcome(Outcome::Failed));
            span.fail(code);
            span.end();
        }
    }

    /// Close the step with `result`: completed, or failed with `code`.
    pub fn close<T, E>(self, result: &Result<T, E>, code: impl Fn(&E) -> &'static str) {
        match result {
            Ok(_) => self.completed(),
            Err(error) => self.fail(code(error)),
        }
    }
}

impl Drop for Step {
    fn drop(&mut self) {
        if let Some(mut span) = self.span.take() {
            span.set(Attr::Outcome(Outcome::Failed));
            span.end();
        }
    }
}

/// The context of the step whose work this task is running, if any.
pub fn current_step() -> Option<SpanCx> {
    STEP.try_with(|cx| *cx).ok()
}

/// At most this many operator activation roots are held open at once; past
/// it the oldest idle one ends as `uncertain`.
pub const MAX_OPERATOR_ROOTS: usize = 256;

/// The controller's tracing and the activations being traced now. Cheap to
/// clone; every clone is the same state.
#[derive(Clone, Default)]
pub struct ActivationTraces {
    inner: Arc<Inner>,
}

#[derive(Default)]
struct Inner {
    tracing: RwLock<Tracing>,
    /// Deployment → the span its lifecycle steps hang under now: a
    /// request-driven or switch activation's root, or a victim's release.
    bound: Mutex<HashMap<String, SpanCx>>,
    /// Operation → the root the worker opened for an activation nothing
    /// bound (an operator's start or wake), with how many of its instance
    /// tasks run now.
    operator: Mutex<HashMap<String, OperatorRoot>>,
}

struct OperatorRoot {
    root: Span,
    running: usize,
}

impl std::fmt::Debug for ActivationTraces {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "ActivationTraces({:?})", self.tracing())
    }
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

impl ActivationTraces {
    /// The role's tracing (`Tracing::off()` until set).
    pub fn set_tracing(&self, tracing: Tracing) {
        *self
            .inner
            .tracing
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = tracing;
    }

    pub fn tracing(&self) -> Tracing {
        self.inner
            .tracing
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
    }

    pub fn is_on(&self) -> bool {
        self.inner
            .tracing
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .is_on()
    }

    /// ADR 0037 §4: a new activation trace's root, recorded whatever the
    /// sampling ratio (§9).
    pub fn root(&self, kind: ActivationKind, deployment: &str) -> Span {
        let mut root = self.tracing().span(Name::Activation, Parent::Root, &[]);
        root.set(Attr::ActivationKind(kind));
        root.set(Attr::Deployment(deployment.to_owned()));
        root
    }

    /// A step under `parent`; empty without one.
    pub fn step(&self, parent: Option<SpanCx>, name: Name) -> Step {
        Step::new(match parent {
            Some(cx) => self.tracing().span(name, Parent::Cx(cx), &[]),
            None => Span::none(),
        })
    }

    /// Until the guard drops, every lifecycle step for `deployment` hangs
    /// under `parent`.
    pub fn bind(&self, deployment: &str, parent: Option<SpanCx>) -> Bound {
        let Some(parent) = parent else {
            return Bound::default();
        };
        lock(&self.inner.bound).insert(deployment.to_owned(), parent);
        Bound {
            traces: Some(self.clone()),
            deployment: deployment.to_owned(),
            parent: Some(parent),
        }
    }

    /// The span `deployment`'s steps hang under now, if one is bound.
    pub fn bound(&self, deployment: &str) -> Option<SpanCx> {
        lock(&self.inner.bound).get(deployment).copied()
    }

    /// Where one worker task's steps for `operation` of `deployment` hang:
    /// under the bound span, or, for an activation (`kind`) nothing bound,
    /// under the operation's own root, opened by its first task and ended by
    /// its last (ADR 0037 §4: an operator's activation is traced the same
    /// way, without waiting requests). Neither for anything else.
    pub fn enter(
        &self,
        deployment: &str,
        operation: &str,
        kind: Option<ActivationKind>,
    ) -> StepParent {
        let mut entered = StepParent {
            traces: self.clone(),
            parent: None,
            operation: None,
            outcome: Mutex::new(None),
        };
        if !self.is_on() {
            return entered;
        }
        if let Some(parent) = self.bound(deployment) {
            entered.parent = Some(parent);
            return entered;
        }
        let Some(kind) = kind else {
            return entered;
        };
        let mut roots = lock(&self.inner.operator);
        if !roots.contains_key(operation) && roots.len() >= MAX_OPERATOR_ROOTS {
            let idle = roots
                .iter()
                .find(|(_, r)| r.running == 0)
                .map(|(id, _)| id.clone());
            match idle.and_then(|id| roots.remove(&id)) {
                Some(mut old) => {
                    old.root.set(Attr::Outcome(Outcome::Uncertain));
                    old.root.end();
                }
                None => return entered,
            }
        }
        let root = roots
            .entry(operation.to_owned())
            .or_insert_with(|| OperatorRoot {
                root: self.root(kind, deployment),
                running: 0,
            });
        root.running += 1;
        entered.parent = root.root.cx();
        entered.operation = Some(operation.to_owned());
        entered
    }
}

/// A deployment bound to an activation's span; unbound on drop.
#[derive(Default)]
pub struct Bound {
    traces: Option<ActivationTraces>,
    deployment: String,
    parent: Option<SpanCx>,
}

impl Drop for Bound {
    fn drop(&mut self) {
        let Some(traces) = &self.traces else { return };
        let mut bound = lock(&traces.inner.bound);
        // Only this binding's: a later one for the deployment is its own.
        if bound.get(&self.deployment).copied() == self.parent {
            bound.remove(&self.deployment);
        }
    }
}

/// Where one worker task's steps hang ([`ActivationTraces::enter`]).
pub struct StepParent {
    traces: ActivationTraces,
    parent: Option<SpanCx>,
    /// Set when the task holds an operator root.
    operation: Option<String>,
    outcome: Mutex<Option<Outcome>>,
}

impl StepParent {
    pub fn cx(&self) -> Option<SpanCx> {
        self.parent
    }

    /// A step under this task's activation; empty without one.
    pub fn step(&self, name: Name) -> Step {
        self.traces.step(self.parent, name)
    }

    /// How the task's work ended; `None` (the default) when it runs again
    /// later (deferred), which keeps an operator root open.
    pub fn finish(&self, outcome: Option<Outcome>) {
        *lock(&self.outcome) = outcome;
    }

    /// The outcome recorded so far.
    pub fn outcome(&self) -> Option<Outcome> {
        *lock(&self.outcome)
    }
}

impl Drop for StepParent {
    fn drop(&mut self) {
        let Some(operation) = self.operation.take() else {
            return;
        };
        let outcome = self.outcome();
        let mut roots = lock(&self.traces.inner.operator);
        let Some(root) = roots.get_mut(&operation) else {
            return;
        };
        root.running = root.running.saturating_sub(1);
        if root.running > 0 {
            return;
        }
        // Without an outcome its last task runs again: the root stays open.
        if let Some(outcome) = outcome {
            if let Some(mut done) = roots.remove(&operation) {
                done.root.set(Attr::Outcome(outcome));
                done.root.end();
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use capyctl_trace::testing::capture;
    use capyctl_trace::{CallerMode, Service};

    // T48 (ADR 0037 §4): an activation nothing bound gets its own root,
    // shared by every instance task of the operation and ended by the last
    // with its outcome; a deferred task keeps it open.
    #[tokio::test]
    async fn an_operator_activation_is_its_own_trace() {
        let (tracing, spans) = capture(0.0, CallerMode::Parent, Service::Server);
        let traces = ActivationTraces::default();
        traces.set_tracing(tracing);
        let first = traces.enter("d", "op-1", Some(ActivationKind::Wake));
        let second = traces.enter("d", "op-1", Some(ActivationKind::Wake));
        assert_eq!(first.cx(), second.cx());
        first.step(Name::Wake).completed();
        first.finish(Some(Outcome::Completed));
        drop(first);
        drop(second);
        // The deferred one keeps the root open.
        assert!(spans.named("capyctl.activation").is_empty());
        let again = traces.enter("d", "op-1", Some(ActivationKind::Wake));
        again.finish(Some(Outcome::Completed));
        drop(again);
        let root = spans.named("capyctl.activation");
        assert_eq!(root.len(), 1, "recorded at ratio 0");
        assert_eq!(root[0].parent, None);
        assert_eq!(root[0].attr("capyctl.activation.kind"), Some("wake"));
        assert_eq!(root[0].attr("capyctl.outcome"), Some("completed"));
        let wake = spans.named("capyctl.wake");
        assert_eq!(wake[0].parent, Some(root[0].cx.span_id));
        assert_eq!(wake[0].cx.trace_id, root[0].cx.trace_id);
        // Not an activation, nothing bound: no span at all.
        assert_eq!(traces.enter("d", "op-2", None).cx(), None);
        assert!(spans.disallowed_keys().is_empty());
    }

    // T48 (ADR 0037 §4, §6): a bound deployment's steps hang under the bound
    // span; a step's work sees the step's context, and nothing else does.
    #[tokio::test]
    async fn bound_steps_and_their_context() {
        let (tracing, spans) = capture(1.0, CallerMode::Parent, Service::Server);
        let traces = ActivationTraces::default();
        traces.set_tracing(tracing);
        let root = traces.root(ActivationKind::ColdStart, "d");
        let bound = traces.bind("d", root.cx());
        let entered = traces.enter("d", "op", Some(ActivationKind::ColdStart));
        assert_eq!(entered.cx(), root.cx());
        let launch = entered.step(Name::Launch);
        let seen = within(launch.cx(), async { current_step() }).await;
        assert_eq!(seen, launch.cx());
        assert_eq!(current_step(), None);
        launch.completed();
        drop(entered);
        drop(bound);
        assert_eq!(traces.bound("d"), None);
        root.end();
        let launch = &spans.named("capyctl.launch")[0];
        let root = &spans.named("capyctl.activation")[0];
        assert_eq!(launch.parent, Some(root.cx.span_id));
    }

    // T48 (ADR 0037 §1): off, nothing is bound, entered or recorded.
    #[tokio::test]
    async fn off_costs_nothing() {
        let traces = ActivationTraces::default();
        let root = traces.root(ActivationKind::Wake, "d");
        assert_eq!(root.cx(), None);
        let _bound = traces.bind("d", root.cx());
        assert_eq!(traces.bound("d"), None);
        assert_eq!(
            traces.enter("d", "op", Some(ActivationKind::Wake)).cx(),
            None
        );
    }
}
