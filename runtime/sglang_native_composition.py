"""Ordered fail-closed composition of the audited SGLang startup gates.

This orchestrates existing runtime observations only. It never imports a native
package, never changes the device or environment, and never grants launch
authority. The returned contract records what was observed at one moment; it is
not durability, qualification, or an entrypoint. Protected package/metadata/
search roots, worker enrollment, and the guarded import boundary remain the
launcher's separate obligations. Placement is mandatory per the audited
contract: an unasserted digest is carried explicitly as placement_asserted=False
so a later caller must require it, never inferred as satisfaction.

The observation attachment here is transport only. Composition does not read
engine state, install a scheduler bridge, or prove readiness, idleness, release,
residency, or qualification of anything.
"""

from dataclasses import dataclass, field
import time

from . import sglang_device as _device
from . import sglang_observation_server as _server
from . import sglang_saver_binding as _saver
from .checkpoint_preflight import revalidate_checkpoint
from .sglang_device import observe_placement
from .sglang_server_args import _validated_public
from .sglang_source_preflight import revalidate_sglang_sources, verify_sglang_sources
from .sglang_startup_guards import enforce_closed_plugins


_CODES = frozenset({
    "source_revalidation_failed", "plugin_closure_failed", "placement_failed",
    "checkpoint_revalidation_failed", "observation_attach_failed",
})
_SOCKET_LEAF = "observe.sock"


class NativeCompositionError(Exception):
    """One closed category per failed gate; never paths or native details."""

    def __init__(self, code):
        self.code = code if code in _CODES else "observation_attach_failed"
        super().__init__(self.code)


@dataclass(frozen=True, repr=False)
class NativeContract:
    """Scoped verified facts at one moment; never launch or admission authority.

    placement_asserted=False records that the service supplied no authorized
    digest. That is an explicit fact for a later caller to require, never a
    pass. Observed placement and checkpoint identities stay private: the
    contract never renders roots, UUIDs, or artifact paths.
    """

    sources_ok: bool
    plugins_closed: bool
    placement_asserted: bool
    placement_digest: str | None
    placement: object = field(repr=False)
    checkpoint: object = field(repr=False)
    binding_id: str
    incarnation: str
    observed_at: float

    def __repr__(self):
        return ("NativeContract(sources_ok=True, plugins_closed=True, "
                f"placement_asserted={self.placement_asserted}, "
                f"binding_id={self.binding_id!r}, incarnation={self.incarnation!r})")


@dataclass(frozen=True, repr=False)
class ObservationHandle:
    """Retained transport custody for the listener path; close before discarding."""

    _path: str = field(repr=False)
    _server: object = field(repr=False)

    @property
    def socket_path(self):
        return self._path

    def close(self):
        try:
            self._server.close()
        except Exception:
            # Shutdown failure retains server custody; the caller never learns
            # a path from us either way.
            raise NativeCompositionError("observation_attach_failed") from None


def compose(spec, checkpoint, *, package_root, trusted_mapping, placement_digest):
    """Run the audited gates in order; any failure denies the whole contract.

    package_root is the trusted installed sglang/srt location selected by the
    launcher, never discovered from the environment. trusted_mapping and
    placement_digest come from service policy; this module never invents them.
    A None digest leaves the mandatory placement gate explicitly unasserted
    (placement_asserted=False); None is never passed downstream as an
    authorized value. checkpoint must already be verified by the entry's main;
    it is revalidated here, immediately before any later protected effect.
    A malformed launch spec denies at the first gate, before anything runs.
    """
    binding_id, incarnation = _step("source_revalidation_failed", _identity, spec)
    previous = _step("source_revalidation_failed", verify_sglang_sources, package_root)
    _step("source_revalidation_failed", revalidate_sglang_sources, previous)
    _step("plugin_closure_failed", enforce_closed_plugins)
    placement, digest = _placement(spec, trusted_mapping, placement_digest)
    verified = _step("checkpoint_revalidation_failed", revalidate_checkpoint, checkpoint)
    return NativeContract(
        sources_ok=True, plugins_closed=True,
        placement_asserted=placement is not None, placement_digest=digest,
        placement=placement, checkpoint=verified, binding_id=binding_id,
        incarnation=incarnation, observed_at=time.time())


def enroll_and_observe(contract, *, bridge, scheduler_identity, controller_identity,
                       socket_dir):
    """Attach the protected observation listener for an enrolled scheduler.

    Requires the exact current scheduler identity (this process, verified
    against trusted procfs) and a separately enrolled live controller identity;
    the server rechecks both before creating anything. The socket is created
    only under the caller-supplied canonical service-owned 0700 directory;
    existing files are never adopted or repaired. The bridge is the scheduler's
    own installed bridge; composition never constructs or reads one.
    """
    path = None
    try:
        if type(contract) is not NativeContract:
            raise NativeCompositionError("observation_attach_failed") from None
        owner = _saver.current_process_identity()
        if scheduler_identity != owner:
            raise NativeCompositionError("observation_attach_failed") from None
        peer = _server._process_identity(controller_identity.pid)
        if peer != controller_identity:
            raise NativeCompositionError("observation_attach_failed") from None
        path = socket_dir.rstrip("/") + "/" + _SOCKET_LEAF
        server = _server.SchedulerObservationServer.start(
            path=path, bridge=bridge, binding_id=contract.binding_id,
            incarnation_id=contract.incarnation, expected_owner=owner,
            expected_peer=peer)
    except NativeCompositionError:
        raise
    except Exception:
        raise NativeCompositionError("observation_attach_failed") from None
    return ObservationHandle(_path=path, _server=server)


def _step(code, operation, *arguments):
    """Fold every underlying closed category into the composition's one code."""
    try:
        return operation(*arguments)
    except NativeCompositionError:
        raise
    except Exception:
        raise NativeCompositionError(code) from None


def _placement(spec, trusted_mapping, digest):
    if digest is None:
        # Mandatory gate, unasserted by the caller: carry the fact, never fake it.
        return None, None
    if trusted_mapping is None or trusted_mapping.inventory_digest != digest:
        raise NativeCompositionError("placement_failed") from None
    try:
        return observe_placement(spec, trusted_mapping), digest
    except Exception:
        raise NativeCompositionError("placement_failed") from None


def _identity(spec):
    # Reuse the entry's entire strict descriptor boundary; no descriptor I/O.
    public = _validated_public(spec)
    return public["binding_id"], public["incarnation"]
