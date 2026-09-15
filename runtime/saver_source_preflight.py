"""Observe the pinned saver release's Python sources without importing it.

This covers only the fixed Python inventory, not compiled binaries, a local
observer patch, native worker provenance, or memory release/restore evidence.
Same-service-user mutation after observation remains outside this guarantee.
"""

from dataclasses import dataclass, field

from . import sglang_source_preflight as _shared


_RELEASE = "0.0.9.post1"
_ARCHIVE_SHA256 = "25fd4b691ed3242c3a18b2bef0dbe9de84d2e7068b96a37686a923d55c274f43"
_SOURCES = (
    ("__init__.py", "f6253f4254dee8ec4c479fceb524549afafa96dce4d78f468e88d8bc6ef8b1a8"),
    ("binary_wrapper.py", "31e4253c722e17271a99c5a9de43978d564c8842bc1ce36e4b1e25f7627893f4"),
    ("entrypoint.py", "685920aa759123cbc5f047802f816f218df774d56d21b50c93578cdffc47f9fb"),
    ("hooks/__init__.py", "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"),
    ("hooks/base.py", "7fc46ffd7c4dba953cd1d29652013e11ae69bb5a9aa17727ce5b59ac3cbdd216"),
    ("hooks/mode_preload.py", "10b69596823b4feae3b2cc67ea5ba36a49cd7ee8f1141c40da873448eb2251f9"),
    ("hooks/mode_torch.py", "cb11b203c018aca5fb86bdc16b0aff307d803304a05f12cdefa1996806d02c65"),
    ("testing_utils.py", "bddf6384c8012db784cb7bddbdbb828e3b4efa54525fe45507f0e0fb04875503"),
    ("utils.py", "e61e958ec31691cdac2d206bb03e3bbe81414b56f9f46a06f284ee3f3d977a26"),
)


class SaverSourcePreflightError(_shared.SourcePreflightError):
    """Closed, path-free category for the saver source inventory."""


@dataclass(frozen=True)
class VerifiedSaverSources:
    release: str
    source_archive_sha256: str
    directories: tuple
    _root: str = field(repr=False)


def _observe_sources(root, inventory):
    """Private synthetic fixture seam, never caller-selected production inventory."""
    try:
        directories = _shared._observe_source_directories(root, inventory)
    except _shared.SourcePreflightError as error:
        raise SaverSourcePreflightError(error.code) from None
    return VerifiedSaverSources(_RELEASE, _ARCHIVE_SHA256, directories, root)


def verify_saver_sources(root):
    """Observe nine pinned Python files beneath an installed torch_memory_saver root."""
    return _observe_sources(root, _SOURCES)


def revalidate_saver_sources(previous):
    """Require the same pinned bytes and protected filesystem identities."""
    if type(previous) is not VerifiedSaverSources:
        raise SaverSourcePreflightError("invalid_root")
    current = verify_saver_sources(previous._root)
    if current != previous:
        raise SaverSourcePreflightError("artifact_changed")
    return current
