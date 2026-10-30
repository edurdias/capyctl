//! #226: on a unified-memory host, the page cache of the checkpoint an engine
//! is about to load is dropped right before the engine starts.
//!
//! SPEC §7.2: on unified memory (a GB10, DGX Spark) the GPU and the CPU share
//! one pool, and the page cache lives in it. Admission counts reclaimable page
//! cache as available, which is right for sizing, but the driver allocates a
//! new CUDA context's first buffers physically contiguous without waiting for
//! reclaim, so a context created while most of the pool is page cache can fail
//! with `CUDA_ERROR_OUT_OF_MEMORY` (found live on a GB10, SGLang 0.5.22). Some
//! of that cache can be the launch's own checkpoint, filled by CapyCTL's
//! digest reads or by earlier reads and engine loads. NVIDIA's DGX Spark known
//! issues give flushing the buffer cache as the workaround on unified memory;
//! here only the launch's own checkpoint is given back, with
//! `posix_fadvise(POSIX_FADV_DONTNEED)` on each of its files: unprivileged,
//! and nothing else on the machine is touched.
//!
//! It is best effort. A failure is logged once per launch and never fails or
//! holds the launch. A discrete GPU (ADR 0019: its memory is a `device`
//! domain, host RAM a `distinct` one) is left as it was.

use crate::checkpoint::{CheckpointError, PageCacheDrop};
use capyctl_config::effective::{DomainMemory, EffectiveDeployment};
use std::os::fd::{AsRawFd, BorrowedFd};

/// How one checkpoint file's cached pages are given back: the seam tests use
/// to observe which files a launch advised without reading the kernel's page
/// cache.
pub trait PageCacheAdvice: Send + Sync {
    /// Give back the cached pages of the open regular file `fd`, which is the
    /// checkpoint's `path` (relative to the checkpoint).
    fn dont_need(&self, path: &str, fd: BorrowedFd<'_>) -> std::io::Result<()>;
}

/// `posix_fadvise(fd, 0, 0, POSIX_FADV_DONTNEED)`: the kernel drops the
/// file's clean cached pages. No privilege is needed; a dirty or mapped page
/// stays (an engine already serving from a mapping keeps its pages).
pub struct Fadvise;

impl PageCacheAdvice for Fadvise {
    fn dont_need(&self, _path: &str, fd: BorrowedFd<'_>) -> std::io::Result<()> {
        // SAFETY: `fd` is an open descriptor for the duration of the call,
        // which only advises the kernel and touches no memory of ours.
        match unsafe { libc::posix_fadvise(fd.as_raw_fd(), 0, 0, libc::POSIX_FADV_DONTNEED) } {
            0 => Ok(()),
            errno => Err(std::io::Error::from_raw_os_error(errno)),
        }
    }
}

/// What [`before_launch`] did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    /// The launch does not start on unified memory; nothing was done.
    NotUnified,
    /// Every file was handed to the advice; how many and how many failed.
    Advised(PageCacheDrop),
    /// The checkpoint could not be opened or walked; no file was advised.
    Unwalkable(CheckpointError),
}

/// SPEC §7.2: whether the launch's starting footprint (the allocations its
/// engine makes as it starts) comes out of a unified domain, the pool the page
/// cache shares. A discrete GPU's `device` domain and host RAM's `distinct`
/// domain do not.
pub fn starts_on_unified_memory(effective: &EffectiveDeployment) -> bool {
    effective
        .resources
        .cold
        .allocations
        .iter()
        .any(|allocation| {
            effective
                .host
                .domains
                .get(&allocation.domain)
                .is_some_and(|domain| domain.memory == DomainMemory::Unified)
        })
}

/// #226: drop the page cache of the checkpoint `effective` loads, right before
/// its engine starts, when it starts on unified memory. Best effort: it never
/// fails, and anything that did not work is logged once for the launch,
/// naming the deployment and the closed reason, never a path.
pub fn before_launch(
    effective: &EffectiveDeployment,
    advice: &dyn PageCacheAdvice,
    deployment_id: &str,
) -> Outcome {
    if !starts_on_unified_memory(effective) {
        return Outcome::NotUnified;
    }
    let outcome = effective
        .model
        .require_resolved_path()
        .map_err(|_| CheckpointError::InvalidRoot)
        .and_then(|checkpoint| {
            crate::checkpoint::drop_page_cache(
                effective.checkpoint_store(),
                std::path::Path::new(checkpoint),
                &|path, fd| advice.dont_need(path, fd),
            )
        });
    match outcome {
        Ok(dropped) => {
            if dropped.failed > 0 {
                capyctl_domain::role_log::event(serde_json::json!({
                    "event": "checkpoint_page_cache_kept",
                    "deployment": deployment_id,
                    "advised": dropped.advised,
                    "failed": dropped.failed,
                    "errno": dropped.first_errno,
                }));
            }
            Outcome::Advised(dropped)
        }
        Err(error) => {
            capyctl_domain::role_log::event(serde_json::json!({
                "event": "checkpoint_page_cache_kept",
                "deployment": deployment_id,
                "reason": error.code(),
            }));
            Outcome::Unwalkable(error)
        }
    }
}
