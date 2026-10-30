use std::fs;
use std::io::Write;
use std::path::PathBuf;

use istl::{Bbox, BytesSource, Reader, STL_HEADER_LEN, TRI_SIZE, convert};

fn tmpdir(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("istl-test-{name}-{}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
    dir
}

/// Write a binary STL with `n` triangles. Triangle i has all three vertices at
/// (x=i%grid, y=i/grid, z=0) so we can identify it from its data.
fn write_synthetic_stl(path: &std::path::Path, n: u32, grid: u32) {
    let mut f = fs::File::create(path).unwrap();
    f.write_all(&[0u8; STL_HEADER_LEN]).unwrap();
    f.write_all(&n.to_le_bytes()).unwrap();
    for i in 0..n {
        let x = (i % grid) as f32;
        let y = (i / grid) as f32;
        let z = 0.0f32;
        // normal = (0,0,1)
        let mut tri = [0u8; TRI_SIZE];
        tri[0..4].copy_from_slice(&0f32.to_le_bytes());
        tri[4..8].copy_from_slice(&0f32.to_le_bytes());
        tri[8..12].copy_from_slice(&1f32.to_le_bytes());
        for v in 0..3 {
            let off = 12 + v * 12;
            tri[off..off + 4].copy_from_slice(&x.to_le_bytes());
            tri[off + 4..off + 8].copy_from_slice(&y.to_le_bytes());
            tri[off + 8..off + 12].copy_from_slice(&z.to_le_bytes());
        }
        // attribute byte count (u16) already zero
        f.write_all(&tri).unwrap();
    }
}

#[test]
fn convert_then_size_invariants() {
    let dir = tmpdir("invariants");
    let stl = dir.join("in.stl");
    let istl = dir.join("out.istl");
    let n = 100u32;
    write_synthetic_stl(&stl, n, 10);

    let stats = convert(&stl, &istl, 16).unwrap();
    assert_eq!(stats.triangles, n as u64);
    assert_eq!(stats.groups, ((n as usize + 15) / 16) as u64);

    let file_size = fs::metadata(&istl).unwrap().len();
    let reader = Reader::open(&istl).unwrap();
    assert_eq!(reader.triangle_count, n);
    assert_eq!(reader.index.groups.len() as u64, stats.groups);
    let sum_sizes: u64 = reader.index.groups.iter().map(|g| g.byte_size).sum();
    assert_eq!(sum_sizes, n as u64 * TRI_SIZE as u64);
    // footer_size from the tail must equal the encoded index size, and the
    // total file size must equal header + count + triangles + footer + 16-B tail.
    assert_eq!(reader.footer_size, reader.index.encoded_size());
    assert_eq!(stats.footer_size, reader.footer_size);
    let expected_size = STL_HEADER_LEN as u64 + 4 + sum_sizes + reader.footer_size + 16;
    assert_eq!(file_size, expected_size);
    // footer_offset + footer_size + tail must hit the end of the file exactly.
    assert_eq!(reader.footer_offset + reader.footer_size + 16, file_size);
    // groups are contiguous starting at 84
    let mut cur = (STL_HEADER_LEN + 4) as u64;
    for g in &reader.index.groups {
        assert_eq!(g.byte_start, cur);
        cur += g.byte_size;
    }
    assert_eq!(cur, stats.footer_offset);
}

#[test]
fn query_subset_and_full() {
    let dir = tmpdir("query");
    let stl = dir.join("in.stl");
    let istl = dir.join("out.istl");
    let n = 1000u32;
    let grid = 32u32;
    write_synthetic_stl(&stl, n, grid);
    convert(&stl, &istl, 64).unwrap();
    let reader = Reader::open(&istl).unwrap();

    // Full-volume query returns everything
    let out_all = dir.join("all.stl");
    let s = reader.query(&Bbox::UNIVERSE, &out_all).unwrap();
    assert_eq!(s.triangles_written, n as u64);
    let bytes = fs::read(&out_all).unwrap();
    assert_eq!(bytes.len(), STL_HEADER_LEN + 4 + n as usize * TRI_SIZE);
    let count_in_file = u32::from_le_bytes(
        bytes[STL_HEADER_LEN..STL_HEADER_LEN + 4]
            .try_into()
            .unwrap(),
    );
    assert_eq!(count_in_file, n);

    // Tight bbox should return only some groups (whole-row-group behavior).
    // Pick a 4×4 window in xy.
    let tight = Bbox::from([3.0, 6.0, 3.0, 6.0, -1.0, 1.0]);
    let out_tight = dir.join("tight.stl");
    let s = reader.query(&tight, &out_tight).unwrap();
    assert!(s.groups_matched > 0);
    assert!(s.triangles_written > 0);
    assert!(s.triangles_written <= n as u64);

    // Far-away bbox returns nothing.
    let far = Bbox::from([1e9, 2e9, 1e9, 2e9, 0.0, 1.0]);
    let out_far = dir.join("far.stl");
    let s = reader.query(&far, &out_far).unwrap();
    assert_eq!(s.groups_matched, 0);
    assert_eq!(s.triangles_written, 0);
    let bytes = fs::read(&out_far).unwrap();
    assert_eq!(bytes.len(), STL_HEADER_LEN + 4);
    let cnt = u32::from_le_bytes(
        bytes[STL_HEADER_LEN..STL_HEADER_LEN + 4]
            .try_into()
            .unwrap(),
    );
    assert_eq!(cnt, 0);
}

#[test]
fn reader_works_over_bytes_source() {
    // The same Reader code path that powers `Reader::open` (FileSource) must
    // also work over an arbitrary RangeReader — that's the whole point of the
    // trait. Read a converted .istl into memory and query it via BytesSource.
    let dir = tmpdir("bytes");
    let stl = dir.join("in.stl");
    let istl = dir.join("out.istl");
    let n = 800u32;
    write_synthetic_stl(&stl, n, 20);
    convert(&stl, &istl, 32).unwrap();

    let bytes = fs::read(&istl).unwrap();
    let src = BytesSource::new(bytes);
    let reader = Reader::open_source(src).unwrap();
    assert_eq!(reader.triangle_count, n);

    // Query via the writer-based API (no temp file).
    let qb = Bbox::from([3.0, 7.0, 3.0, 7.0, -1.0, 1.0]);
    let mut buf: Vec<u8> = Vec::new();
    let stats = reader.query_to_writer(&qb, &mut buf).unwrap();
    assert!(stats.groups_matched > 0);
    assert!(stats.triangles_written > 0);
    // Output is a valid binary STL: 80 B header + u32 count + 50·N triangles.
    assert_eq!(
        buf.len() as u64,
        STL_HEADER_LEN as u64 + 4 + stats.triangles_written * TRI_SIZE as u64
    );
    let count_in_file =
        u32::from_le_bytes(buf[STL_HEADER_LEN..STL_HEADER_LEN + 4].try_into().unwrap());
    assert_eq!(count_in_file as u64, stats.triangles_written);
}

#[test]
fn query_istl_round_trip_full() {
    // Bbox::UNIVERSE → output ISTL should round-trip every group from the source.
    let dir = tmpdir("istl-full");
    let stl = dir.join("in.stl");
    let istl = dir.join("out.istl");
    let n = 1000u32;
    write_synthetic_stl(&stl, n, 32);
    convert(&stl, &istl, 64).unwrap();

    let src_reader = Reader::open(&istl).unwrap();
    let out_istl = dir.join("all.istl");
    let stats = src_reader.query_istl(&Bbox::UNIVERSE, &out_istl).unwrap();
    assert_eq!(stats.triangles_written, n as u64);
    assert_eq!(stats.groups_matched as usize, src_reader.index.groups.len());

    let out_reader = Reader::open(&out_istl).unwrap();
    assert_eq!(out_reader.triangle_count, n);
    assert_eq!(out_reader.index.groups.len(), src_reader.index.groups.len());
    // Per-group bbox and size are preserved verbatim.
    for (a, b) in out_reader
        .index
        .groups
        .iter()
        .zip(src_reader.index.groups.iter())
    {
        assert_eq!(a.byte_size, b.byte_size);
        assert_eq!(a.bbox, b.bbox);
    }
    // Footer self-locates correctly and groups are contiguous from 84.
    assert_eq!(out_reader.footer_size, out_reader.index.encoded_size());
    let mut cur = (STL_HEADER_LEN + 4) as u64;
    for g in &out_reader.index.groups {
        assert_eq!(g.byte_start, cur);
        cur += g.byte_size;
    }
    assert_eq!(cur, out_reader.footer_offset);
}

#[test]
fn query_istl_round_trip_subset() {
    let dir = tmpdir("istl-subset");
    let stl = dir.join("in.stl");
    let istl = dir.join("out.istl");
    let n = 1000u32;
    write_synthetic_stl(&stl, n, 32);
    convert(&stl, &istl, 64).unwrap();

    let src_reader = Reader::open(&istl).unwrap();
    let tight = Bbox::from([3.0, 6.0, 3.0, 6.0, -1.0, 1.0]);
    let out_istl = dir.join("tight.istl");
    let stats = src_reader.query_istl(&tight, &out_istl).unwrap();
    assert!(stats.groups_matched > 0);
    assert!((stats.groups_matched as usize) < src_reader.index.groups.len());

    let out_reader = Reader::open(&out_istl).unwrap();
    assert_eq!(out_reader.index.groups.len() as u64, stats.groups_matched);
    assert_eq!(out_reader.footer_size, out_reader.index.encoded_size());
    // byte_starts contiguous from 84.
    let mut cur = (STL_HEADER_LEN + 4) as u64;
    for g in &out_reader.index.groups {
        assert_eq!(g.byte_start, cur);
        cur += g.byte_size;
    }
    assert_eq!(cur, out_reader.footer_offset);

    // Re-querying the output ISTL with the same bbox must return the same
    // triangles — proves the output is a self-consistent indexed file.
    let mut tris_via_output: Vec<u8> = Vec::new();
    out_reader
        .query_to_writer(&tight, &mut tris_via_output)
        .unwrap();
    let mut tris_direct: Vec<u8> = Vec::new();
    src_reader
        .query_to_writer(&tight, &mut tris_direct)
        .unwrap();
    assert_eq!(tris_via_output, tris_direct);
}

#[test]
fn query_istl_empty_match_is_valid() {
    let dir = tmpdir("istl-empty");
    let stl = dir.join("in.stl");
    let istl = dir.join("out.istl");
    write_synthetic_stl(&stl, 200, 10);
    convert(&stl, &istl, 32).unwrap();

    let reader = Reader::open(&istl).unwrap();
    let far = Bbox::from([1e9, 2e9, 1e9, 2e9, 0.0, 1.0]);
    let out = dir.join("empty.istl");
    let stats = reader.query_istl(&far, &out).unwrap();
    assert_eq!(stats.groups_matched, 0);
    assert_eq!(stats.triangles_written, 0);

    // Minimum valid ISTL: 80 header + 4 count + 0 triangles + 4 footer payload + 16 tail.
    let size = fs::metadata(&out).unwrap().len();
    assert_eq!(size, (STL_HEADER_LEN + 4 + 4 + 16) as u64);

    let out_reader = Reader::open(&out).unwrap();
    assert_eq!(out_reader.triangle_count, 0);
    assert_eq!(out_reader.index.groups.len(), 0);
}

#[test]
fn query_istl_via_bytes_source() {
    let dir = tmpdir("istl-bytes");
    let stl = dir.join("in.stl");
    let istl = dir.join("out.istl");
    let n = 800u32;
    write_synthetic_stl(&stl, n, 20);
    convert(&stl, &istl, 32).unwrap();

    let bytes = fs::read(&istl).unwrap();
    let src = BytesSource::new(bytes);
    let reader = Reader::open_source(src).unwrap();

    let qb = Bbox::from([3.0, 7.0, 3.0, 7.0, -1.0, 1.0]);
    let mut buf: Vec<u8> = Vec::new();
    let stats = reader.query_to_writer_istl(&qb, &mut buf).unwrap();
    assert!(stats.groups_matched > 0);

    // Reparse the in-memory result as ISTL.
    let out_src = BytesSource::new(buf);
    let out_reader = Reader::open_source(out_src).unwrap();
    assert_eq!(out_reader.index.groups.len() as u64, stats.groups_matched);
    assert_eq!(out_reader.footer_size, out_reader.index.encoded_size());
}

#[test]
fn brute_force_subset_check() {
    // The query result must be a SUPERSET of any per-triangle filter, since
    // istl returns whole row groups.
    let dir = tmpdir("brute");
    let stl = dir.join("in.stl");
    let istl = dir.join("out.istl");
    let n = 500u32;
    let grid = 25u32;
    write_synthetic_stl(&stl, n, grid);
    convert(&stl, &istl, 32).unwrap();
    let reader = Reader::open(&istl).unwrap();

    let qb = Bbox::from([5.0, 10.0, 5.0, 10.0, -1.0, 1.0]);
    let out = dir.join("q.stl");
    let stats = reader.query(&qb, &out).unwrap();

    // Brute force: count triangles whose first-vertex XY lies in qb.
    let raw = fs::read(&stl).unwrap();
    let mut expected_in = 0u64;
    for i in 0..n as usize {
        let off = STL_HEADER_LEN + 4 + i * TRI_SIZE;
        let x = f32::from_le_bytes(raw[off + 12..off + 16].try_into().unwrap());
        let y = f32::from_le_bytes(raw[off + 16..off + 20].try_into().unwrap());
        if x >= qb.xmin && x <= qb.xmax && y >= qb.ymin && y <= qb.ymax {
            expected_in += 1;
        }
    }
    assert!(
        stats.triangles_written >= expected_in,
        "result {} should superset brute-force in-bbox count {}",
        stats.triangles_written,
        expected_in
    );
}
