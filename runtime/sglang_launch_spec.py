"""Shared private launch value, independent of the wrapper execution name.

SPEC §9.2 / T22: the script and imported helpers must use one nominal type.
Construction alone conveys no launch authority; the entry validates all fields.
"""

from dataclasses import dataclass, field


@dataclass(frozen=True, repr=False)
class LaunchSpec:
    """Private process-local inputs, never a serialized or public DTO."""

    _public_json: str = field(repr=False)
    _checkpoint_root: str = field(repr=False)
    _inference_key: str = field(repr=False)
    _admin_key: str = field(repr=False)
    # Version 1 has no scope and must never be promoted implicitly. Version 2
    # carries immutable private metadata, not enrollment or execution authority,
    # plus the optional service-authorized device inventory digest the host
    # policy published; the entry never invents either.
    _launch_scope_json: str | None = field(default=None, repr=False)
    _placement_digest: str | None = field(default=None, repr=False)

    def __repr__(self):
        return "LaunchSpec(<private>)"

