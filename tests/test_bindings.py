import math
import struct
from pathlib import Path

import istl
import meshio
import pytest


def _stl_triangle_count(path: Path) -> int:
    data = path.read_bytes()
    return struct.unpack_from("<I", data, 80)[0]


def test_convert_round_trip(tmp_stl, tmp_path):
    stl = tmp_stl(n=500, grid=25, name="in.stl")
    out = tmp_path / "out.istl"
    stats = istl.convert(str(stl), str(out), group_size=32)
    assert stats["triangles"] == 500
    assert stats["groups"] == math.ceil(500 / 32)
    raw = out.read_bytes()
    # Output starts with ISTL1 identifier. The remaining 72 bytes of the 80-byte
    # STL header are reserved (zeroed) — the footer pointer is at the tail.
    assert raw[:8] == b"ISTL1\x00\x00\x00"
    assert all(b == 0 for b in raw[8:80])
    # Last 16 bytes = footer_size (u64 LE) + TRAILER identifier.
    tail_size, tail_magic = struct.unpack_from("<Q8s", raw, len(raw) - 16)
    assert tail_magic == b"ISTLEND\x00"
    assert tail_size == stats["footer_size"]
    # footer_offset + footer_size + 16-B tail == file size.
    assert stats["footer_offset"] + stats["footer_size"] + 16 == out.stat().st_size
    # The triangle count field (bytes 80..84) is still correct, so meshio
    # treats it as a plain binary STL.
    assert _stl_triangle_count(out) == 500


def test_read_index_shapes(tmp_stl, tmp_path):
    stl = tmp_stl(n=200, grid=20, name="in.stl")
    out = tmp_path / "out.istl"
    istl.convert(str(stl), str(out), group_size=16)
    idx = istl.read_index(str(out))
    assert len(idx) == math.ceil(200 / 16)
    g0 = idx[0]
    for key in ("byte_start", "byte_size", "xmin", "xmax", "ymin", "ymax", "zmin", "zmax"):
        assert key in g0
    # First group starts at offset 84 (header + count).
    assert g0["byte_start"] == 84
    # All groups together cover exactly 200 * 50 bytes.
    assert sum(g["byte_size"] for g in idx) == 200 * 50


def test_query_subset_is_valid_stl(tmp_stl, tmp_path):
    stl = tmp_stl(n=400, grid=20, name="in.stl")
    indexed = tmp_path / "in.istl"
    sliced = tmp_path / "sliced.stl"
    istl.convert(str(stl), str(indexed), group_size=24)
    stats = istl.query(
        str(indexed),
        str(sliced),
        bbox=(3.0, 3.0, -1.0, 8.0, 8.0, 1.0),
    )
    assert stats["groups_matched"] >= 1
    assert stats["triangles_written"] >= 1
    # meshio can read it back as a normal binary STL.
    mesh = meshio.read(str(sliced))
    n_in_file = _stl_triangle_count(sliced)
    assert n_in_file == stats["triangles_written"]
    assert sum(len(c.data) for c in mesh.cells if c.type == "triangle") == n_in_file


def test_query_superset_of_brute_force(tmp_stl, tmp_path):
    n, grid = 300, 25
    stl = tmp_stl(n=n, grid=grid, name="in.stl")
    indexed = tmp_path / "in.istl"
    sliced = tmp_path / "sliced.stl"
    istl.convert(str(stl), str(indexed), group_size=32)

    qb = (4.0, 4.0, -1.0, 9.0, 9.0, 1.0)
    stats = istl.query(str(indexed), str(sliced), bbox=qb)

    # Brute-force scan original STL by first-vertex XY.
    raw = stl.read_bytes()
    expected = 0
    for i in range(n):
        off = 84 + i * 50 + 12  # skip normal -> first vertex
        x, y = struct.unpack_from("<ff", raw, off)
        if qb[0] <= x <= qb[3] and qb[1] <= y <= qb[4]:
            expected += 1
    assert stats["triangles_written"] >= expected


def test_query_format_istl_round_trips(tmp_stl, tmp_path):
    """`format="istl"` writes a valid `.istl` whose index can be re-queried."""
    stl = tmp_stl(n=400, grid=20, name="in.stl")
    indexed = tmp_path / "in.istl"
    sliced_istl = tmp_path / "sliced.istl"
    sliced_stl = tmp_path / "sliced.stl"
    istl.convert(str(stl), str(indexed), group_size=24)

    qb = (3.0, 3.0, -1.0, 8.0, 8.0, 1.0)
    stats = istl.query(str(indexed), str(sliced_istl), bbox=qb, format="istl")
    assert stats["groups_matched"] >= 1

    # The sliced .istl re-parses through read_index.
    sub_idx = istl.read_index(str(sliced_istl))
    assert len(sub_idx) == stats["groups_matched"]
    assert sub_idx[0]["byte_start"] == 84

    # Re-querying the sliced ISTL with the SAME bbox yields the same triangles
    # as querying the original ISTL — proves the sliced file is self-consistent.
    s_via = istl.query(str(sliced_istl), str(sliced_stl), bbox=qb, format="stl")
    s_direct = istl.query(str(indexed), str(tmp_path / "direct.stl"), bbox=qb)
    assert s_via["triangles_written"] == s_direct["triangles_written"]
    assert (
        (tmp_path / "direct.stl").read_bytes()
        == sliced_stl.read_bytes()
    )


def test_query_format_rejects_unknown(tmp_stl, tmp_path):
    stl = tmp_stl(n=50, grid=10, name="in.stl")
    indexed = tmp_path / "in.istl"
    istl.convert(str(stl), str(indexed), group_size=16)
    with pytest.raises(ValueError):
        istl.query(
            str(indexed),
            str(tmp_path / "x.stl"),
            bbox=(0.0, 0.0, -1.0, 10.0, 10.0, 1.0),
            format="ply",
        )


def test_query_format_default_is_stl(tmp_stl, tmp_path):
    """No `format=` kwarg should preserve the pre-change behavior (STL out)."""
    stl = tmp_stl(n=100, grid=10, name="in.stl")
    indexed = tmp_path / "in.istl"
    sliced = tmp_path / "sliced.bin"
    istl.convert(str(stl), str(indexed), group_size=16)
    istl.query(
        str(indexed),
        str(sliced),
        bbox=(0.0, 0.0, -1.0, 10.0, 10.0, 1.0),
    )
    raw = sliced.read_bytes()
    # Vanilla binary STL: 80-byte zero header (no ISTL identifier).
    assert raw[:8] != b"ISTL1\x00\x00\x00"
    # No ISTLEND trailer.
    assert raw[-8:] != b"ISTLEND\x00"


def test_query_rejects_inverted_bbox(tmp_stl, tmp_path):
    stl = tmp_stl(n=50, grid=10, name="in.stl")
    indexed = tmp_path / "in.istl"
    istl.convert(str(stl), str(indexed), group_size=16)
    with pytest.raises(ValueError):
        istl.query(str(indexed), str(tmp_path / "x.stl"), bbox=(5.0, 0.0, 0.0, 0.0, 1.0, 1.0))
