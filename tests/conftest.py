import struct
from pathlib import Path

import pytest


def write_synthetic_stl(path: Path, n: int, grid: int) -> None:
    """Write a binary STL whose triangle i has all 3 vertices at
    (x=i%grid, y=i//grid, z=0). Useful for deterministic round-trip tests."""
    with path.open("wb") as f:
        f.write(b"\x00" * 80)
        f.write(struct.pack("<I", n))
        for i in range(n):
            x, y, z = float(i % grid), float(i // grid), 0.0
            f.write(struct.pack("<fff", 0.0, 0.0, 1.0))           # normal
            for _ in range(3):
                f.write(struct.pack("<fff", x, y, z))             # 3 vertices
            f.write(b"\x00\x00")                                   # attr u16


@pytest.fixture
def tmp_stl(tmp_path: Path):
    """Returns a (factory) that writes a synthetic STL and returns its path."""

    def _make(n: int = 1000, grid: int = 32, name: str = "synth.stl") -> Path:
        p = tmp_path / name
        write_synthetic_stl(p, n, grid)
        return p

    return _make
