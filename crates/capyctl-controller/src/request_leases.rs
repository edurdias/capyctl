//! Durable request leases for router dispatch (SPEC §10, owner decision
//! 2026-09-22).
//!
//! SPEC §10: queued bodies and open streams are not promised to survive a server
//! crash, but a client disconnect is not proof the engine stopped working, so
//! conservative accounting is kept until cancellation acknowledgement, completion
//! observation, or controlled cleanup. The router therefore writes one durable
//! lease per dispatch before anything is sent to an engine, and closes it only on
//! evidence: the backend completed, the backend acknowledged a cancellation, or the
//! request provably never reached the engine. Anything else marks the lease
//! uncertain and leaves it charged. Nothing here closes a lease on a timer; the
//! leases a crashed session leaves behind are settled by W12's quiescence path
//! (`abandon_retired_request_leases`) or by a controlled cleanup's gone evidence.
//!
//! Writes are group-committed. One writer thread owns the store access: it takes
//! every write queued while the previous commit was running, up to `MAX_BATCH`,
//! and applies them in one transaction. No write waits for a timer to fill a
//! batch, so the latency a dispatch pays is one commit plus whatever commit was
//! already in progress. Grants are bounded (`MAX_QUEUED_GRANTS`) and refused
//! rather than queued without limit; closes are never refused, because each one
//! corresponds to a lease already granted and the grants are bounded.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{mpsc, Arc, Mutex, PoisonError};
use std::time::Duration;

use capyctl_store::dispatch::{DispatchError, DispatchTicket, LeaseWrite, LeaseWriteOutcome};

use crate::fault::LifecycleFault;

/// The most writes applied in one transaction.
pub const MAX_BATCH: usize = 128;
/// Grants waiting for the writer beyond this are refused as saturated. It bounds
/// the latency a queued grant can accumulate behind earlier batches.
pub const MAX_QUEUED_GRANTS: usize = 1024;
/// The bound on outstanding leases across every deployment, inflight and
/// uncertain together.
pub const MAX_TOTAL_LEASES: usize = 4096;
/// How long an idle writer waits before it looks for orphaned grants again.
const ORPHAN_POLL: Duration = Duration::from_millis(50);

/// One open durable lease. Not `Clone`: a lease is closed exactly once, by
/// whoever holds it, and dropping it without closing leaves it charged.
#[must_use = "an open request lease is retained backend work until it is closed on evidence"]
#[derive(Debug)]
pub struct RequestLease {
    id: String,
    /// `None` only for a lease minted by a test authority that has no store;
    /// the durable writer refuses to settle one.
    ticket: Option<DispatchTicket>,
}

impl RequestLease {
    fn durable(ticket: DispatchTicket) -> Self {
        Self {
            id: ticket.id().to_owned(),
            ticket: Some(ticket),
        }
    }

    /// A lease with no durable record, for a test authority that stands a router
    /// up without a store. Not for production: nothing about it is durable.
    #[doc(hidden)]
    pub fn unrecorded(id: &str) -> Self {
        Self {
            id: id.to_owned(),
            ticket: None,
        }
    }

    pub fn id(&self) -> &str {
        &self.id
    }

    /// SPEC §6.5 (W5): the instance incarnation this lease is charged to,
    /// `(deployment, generation)`, for the idle timers. `None` when unrecorded.
    pub fn instance(&self) -> Option<(String, i64)> {
        self.ticket
            .as_ref()
            .map(|ticket| (ticket.deployment_id().to_owned(), ticket.generation()))
    }
}

/// How a dispatch ended, as far as the evidence shows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LeaseEnd {
    /// The backend reported its protocol terminator, or acknowledged a
    /// cancellation. Closes the lease.
    Completed,
    /// The request provably never reached the engine (for example a host ingress
    /// refused it before forwarding while shutting down). Closes the lease.
    NotAccepted,
    /// Anything else. The lease stays charged, marked uncertain.
    Uncertain,
}

/// Why a lease was not granted.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum LeaseRefused {
    /// The deployment's dispatch gate is closed or its fence moved. Retryable.
    #[error("dispatch is closed for this deployment")]
    Closed,
    /// An outstanding-request bound was reached.
    #[error("outstanding request bound reached")]
    Full,
    /// The ledger could not answer; nothing was granted.
    #[error("request ledger unavailable: {0}")]
    Unavailable(String),
}

type Outcomes = Result<Vec<Result<LeaseWriteOutcome, DispatchError>>, LifecycleFault>;

/// Where a batch is applied. Production applies it to the coordinator's owned
/// store under its current session; tests may supply a store directly.
pub type LeaseBackend = Arc<dyn Fn(&[LeaseWrite]) -> Outcomes + Send + Sync>;

/// Closes for grants nobody took, applied ahead of the writer's next batch.
type Orphans = Arc<Mutex<Vec<LeaseWrite>>>;

/// A granted ticket on its way to the caller. SPEC §10: a grant whose caller
/// went away before taking it was never handed to the router, so nothing was
/// dispatched under it. That is the evidence `NotAccepted` names, and dropping
/// an untaken ticket queues its close. This covers a reply the writer could
/// not send (the caller had already gone) and one it sent that the caller
/// never polled (its future was dropped in between, for example on a client
/// disconnect), which would otherwise stay charged until the session retires.
struct Undelivered {
    ticket: Option<DispatchTicket>,
    orphans: Orphans,
}

impl Undelivered {
    fn take(mut self) -> Option<DispatchTicket> {
        self.ticket.take()
    }
}

impl Drop for Undelivered {
    fn drop(&mut self) {
        if let Some(ticket) = self.ticket.take() {
            self.orphans
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .push(LeaseWrite::Finish(ticket));
        }
    }
}

/// The writer's answer to one job.
enum Answer {
    Granted(Undelivered),
    Settled,
}

struct Job {
    write: LeaseWrite,
    reply: tokio::sync::oneshot::Sender<Result<Answer, LeaseRefused>>,
}

/// The group-commit writer. Dropping every handle ends its thread once the queue
/// drains.
pub struct RequestLeaseWriter {
    jobs: Option<mpsc::Sender<Job>>,
    queued_grants: Arc<AtomicUsize>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl Drop for RequestLeaseWriter {
    /// Drain what is queued and join the thread, so whatever the backend holds
    /// (the coordinator's owned state and its process lock) is released by the
    /// time the writer is gone rather than some moments later.
    fn drop(&mut self) {
        self.jobs.take();
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

impl RequestLeaseWriter {
    pub fn spawn(backend: LeaseBackend) -> Self {
        let (jobs, receiver) = mpsc::channel::<Job>();
        let queued_grants = Arc::new(AtomicUsize::new(0));
        let counter = queued_grants.clone();
        // A dedicated thread keeps blocking SQLite commits off the async
        // executor, and lets writes that arrive during a commit form the next
        // group without any timer.
        let spawned = std::thread::Builder::new()
            .name("capyctl-request-leases".into())
            .spawn(move || run(receiver, backend, counter));
        let thread = match spawned {
            Ok(thread) => Some(thread),
            Err(_) => {
                // The receiver was dropped with the closure: every write is then
                // refused as unavailable rather than silently skipped.
                capyctl_domain::role_log::notice(
                    capyctl_domain::role_log::Level::Warning,
                    "request lease writer could not start; dispatch will be refused",
                );
                None
            }
        };
        Self {
            jobs: Some(jobs),
            queued_grants,
            thread,
        }
    }

    /// Open a lease against the deployment's current fence.
    pub async fn open(
        &self,
        deployment: &str,
        max_per_deployment: usize,
    ) -> Result<RequestLease, LeaseRefused> {
        self.grant(LeaseWrite::Grant {
            deployment_id: deployment.to_owned(),
            max_per_deployment,
            max_total: MAX_TOTAL_LEASES,
        })
        .await
    }

    /// ADR 0013 §10 (I3): open a lease charged to exactly the instance
    /// incarnation `generation` names. Refused as `Closed` when that instance's
    /// gate is shut or it moved to another generation since the router chose
    /// it, so the router fails over without anything having been sent.
    pub async fn open_instance(
        &self,
        deployment: &str,
        generation: i64,
        max_per_deployment: usize,
    ) -> Result<RequestLease, LeaseRefused> {
        self.grant(LeaseWrite::GrantInstance {
            deployment_id: deployment.to_owned(),
            generation,
            max_per_deployment,
            max_total: MAX_TOTAL_LEASES,
        })
        .await
    }

    async fn grant(&self, write: LeaseWrite) -> Result<RequestLease, LeaseRefused> {
        if self.queued_grants.fetch_add(1, Ordering::SeqCst) >= MAX_QUEUED_GRANTS {
            self.queued_grants.fetch_sub(1, Ordering::SeqCst);
            return Err(LeaseRefused::Unavailable(
                "request ledger is saturated".into(),
            ));
        }
        match self.submit(write).await {
            Ok(Answer::Granted(granted)) => granted
                .take()
                .map(RequestLease::durable)
                .ok_or_else(|| LeaseRefused::Unavailable("unexpected ledger answer".into())),
            Ok(Answer::Settled) => {
                Err(LeaseRefused::Unavailable("unexpected ledger answer".into()))
            }
            Err(refused) => Err(refused),
        }
    }

    /// Close or retain a lease according to the evidence. An error means the
    /// write did not land and the lease stays exactly as it was: charged.
    pub async fn close(&self, lease: RequestLease, end: LeaseEnd) -> Result<(), LeaseRefused> {
        let Some(ticket) = lease.ticket else {
            return Err(LeaseRefused::Unavailable("not a durable lease".into()));
        };
        let write = match end {
            LeaseEnd::Completed | LeaseEnd::NotAccepted => LeaseWrite::Finish(ticket),
            LeaseEnd::Uncertain => LeaseWrite::Uncertain(ticket),
        };
        self.submit(write).await.map(|_| ())
    }

    async fn submit(&self, write: LeaseWrite) -> Result<Answer, LeaseRefused> {
        let grant = is_grant(&write);
        let (reply, answer) = tokio::sync::oneshot::channel();
        let sent = self
            .jobs
            .as_ref()
            .is_some_and(|jobs| jobs.send(Job { write, reply }).is_ok());
        if !sent {
            if grant {
                self.queued_grants.fetch_sub(1, Ordering::SeqCst);
            }
            return Err(LeaseRefused::Unavailable("request ledger stopped".into()));
        }
        // The writer answers every job it takes, including on a failed batch; a
        // dropped reply means the writer is gone, and the write's fate is then
        // unknown. For a grant that is harmless (nothing is dispatched without
        // the ticket, and an untaken ticket queues its own close); a close that
        // did not answer leaves the lease charged.
        answer
            .await
            .unwrap_or_else(|_| Err(LeaseRefused::Unavailable("request ledger stopped".into())))
    }
}

fn is_grant(write: &LeaseWrite) -> bool {
    matches!(
        write,
        LeaseWrite::Grant { .. } | LeaseWrite::GrantInstance { .. }
    )
}

fn refusal(error: DispatchError) -> LeaseRefused {
    match error {
        DispatchError::Closed | DispatchError::Conflict => LeaseRefused::Closed,
        DispatchError::Full => LeaseRefused::Full,
        other => LeaseRefused::Unavailable(other.to_string()),
    }
}

fn run(receiver: mpsc::Receiver<Job>, backend: LeaseBackend, queued_grants: Arc<AtomicUsize>) {
    // SPEC §10: closes for grants whose caller went away before taking the
    // ticket. They join the next batch ahead of anything newly queued.
    let orphans: Orphans = Arc::default();
    let mut disconnected = false;
    loop {
        let pending = std::mem::take(&mut *orphans.lock().unwrap_or_else(PoisonError::into_inner));
        let mut batch: Vec<Job> = pending
            .into_iter()
            .map(|write| Job {
                write,
                // Nobody waits on an orphan's close; its answer is dropped.
                reply: tokio::sync::oneshot::channel().0,
            })
            .collect();
        if batch.is_empty() {
            if disconnected {
                break;
            }
            // Wake now and then while idle: a ticket can be dropped after its
            // reply was sent, with nothing else queued behind it.
            match receiver.recv_timeout(ORPHAN_POLL) {
                Ok(first) => batch.push(first),
                Err(mpsc::RecvTimeoutError::Timeout) => continue,
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    // Flush what is orphaned before the thread ends.
                    disconnected = true;
                    continue;
                }
            }
        }
        while batch.len() < MAX_BATCH {
            match receiver.try_recv() {
                Ok(job) => batch.push(job),
                Err(_) => break,
            }
        }
        let grants = batch.iter().filter(|job| is_grant(&job.write)).count();
        let writes: Vec<LeaseWrite> = batch.iter().map(|job| job.write.clone()).collect();
        let outcomes = backend(&writes);
        queued_grants.fetch_sub(grants, Ordering::SeqCst);
        match outcomes {
            Ok(outcomes) if outcomes.len() == batch.len() => {
                for (job, outcome) in batch.into_iter().zip(outcomes) {
                    let answer = match outcome {
                        Ok(LeaseWriteOutcome::Granted(ticket)) => {
                            Ok(Answer::Granted(Undelivered {
                                ticket: Some(ticket),
                                orphans: orphans.clone(),
                            }))
                        }
                        Ok(LeaseWriteOutcome::Settled(_)) => Ok(Answer::Settled),
                        Err(error) => Err(refusal(error)),
                    };
                    // A reply that cannot be sent comes back and is dropped
                    // here, which queues a grant's close as an orphan.
                    let _ = job.reply.send(answer);
                }
            }
            Ok(_) => {
                for job in batch {
                    let _ = job.reply.send(Err(LeaseRefused::Unavailable(
                        "ledger answer mismatch".into(),
                    )));
                }
            }
            Err(fault) => {
                let reason = fault.to_string();
                for job in batch {
                    let _ = job
                        .reply
                        .send(Err(LeaseRefused::Unavailable(reason.clone())));
                }
            }
        }
    }
}

#[cfg(test)]
mod tests;
