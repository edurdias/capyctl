"""Compiled-in checkpoint contract; private types also support synthetic tests."""

from dataclasses import dataclass


@dataclass(frozen=True)
class _Artifact:
    name: str
    sha256: str
    size: int | None


@dataclass(frozen=True)
class _Geometry:
    layers: int
    hidden: int
    intermediate: int
    heads: int
    kv_heads: int
    head_dim: int
    vocabulary: int
    maximum_context: int


_PINNED_GEOMETRY = _Geometry(36, 2560, 9728, 32, 8, 128, 151936, 262144)


@dataclass(frozen=True)
class _Manifest:
    artifacts: tuple[_Artifact, ...]
    geometry: _Geometry = _PINNED_GEOMETRY
    index_discrepancy: int = 655360


_PINNED_MANIFEST = _Manifest((
    _Artifact("config.json", "5beea1a4a34c62782bfb2f911c606741a3bab8f92d80a118fa053c28af12e8ba", None),
    _Artifact("model.safetensors.index.json", "d6c42883a895dfef5b0080ed2116a1bcd764f558406b98923d675978a1abf29c", None),
    _Artifact("model-00001-of-00003.safetensors", "75311d91bb08cf0b882913da464a1e722a31fb44db35208663487efb7a3d8ed6", 3957900840),
    _Artifact("model-00002-of-00003.safetensors", "0b48adbb1f60e901153d91907ba11ce63bd4b8b584482e730f48808d055dfba1", 3987450520),
    _Artifact("model-00003-of-00003.safetensors", "7dd39ccca5e4de123c74c14af44c9bf2eb75df33b4614382af0134528e060d5d", 99630640),
    _Artifact("tokenizer_config.json", "a62ff0a2472a0fa1b8eaabcb57c59b58afa42a22831dc141400b6e0cf2b65ce3", None),
    _Artifact("tokenizer.json", "aeb13307a71acd8fe81861d94ad54ab689df773318809eed3cbe794b4492dae4", None),
    _Artifact("vocab.json", "ca10d7e9fb3ed18575dd1e277a2579c16d108e32f27439684afa0e10b1440910", None),
    _Artifact("merges.txt", "599bab54075088774b1733fde865d5bd747cbcc7a547c5bc12610e874e26f5e3", None),
    _Artifact("generation_config.json", "835fffe355c9438e7a25be099b3fccaa98350b83451f9fd2d99512e74f1ade48", None),
))
