use std::fs::File;
use std::io::{self, BufWriter, Write};
use std::path::PathBuf;

use numpy::{PyArray3, PyArrayMethods};
use pyo3::exceptions::{PyIOError, PyValueError};
use pyo3::prelude::*;
use pyo3::types::{PyBytes, PyDict, PyList};

use istl_core::QueryStats;

mod converter;
pub use converter::convert;

fn io_err<E: std::fmt::Display>(e: E) -> PyErr {
    PyIOError::new_err(e.to_string())
}

#[derive(Clone, Copy)]
enum OutputFormat {
    Stl,
    Istl,
}

fn parse_format(s: &str) -> PyResult<OutputFormat> {
    match s {
        "stl" | "STL" => Ok(OutputFormat::Stl),
        "istl" | "ISTL" => Ok(OutputFormat::Istl),
        _ => Err(PyValueError::new_err(format!(
            "format must be \"stl\" or \"istl\", got {s:?}"
        ))),
    }
}

fn parse_bbox(bbox: (f32, f32, f32, f32, f32, f32)) -> PyResult<istl_core::Bbox> {
    let (xmin, ymin, zmin, xmax, ymax, zmax) = bbox;
    if !(xmin <= xmax && ymin <= ymax && zmin <= zmax) {
        return Err(PyValueError::new_err(
            "bbox has min > max on at least one axis",
        ));
    }
    Ok(istl_core::Bbox {
        xmin,
        xmax,
        ymin,
        ymax,
        zmin,
        zmax,
    })
}

enum Endpoint {
    Local(PathBuf),
    Remote(String),
}

fn classify(s: &str) -> Endpoint {
    if let Some(rest) = strip_file_scheme(s) {
        return Endpoint::Local(rest);
    }
    if looks_like_url(s) {
        return Endpoint::Remote(s.to_string());
    }
    Endpoint::Local(PathBuf::from(s))
}

fn looks_like_url(s: &str) -> bool {
    let bytes = s.as_bytes();
    let Some(first) = bytes.first() else {
        return false;
    };
    if !first.is_ascii_alphabetic() {
        return false;
    }
    let mut i = 1;
    while i < bytes.len() {
        let c = bytes[i];
        if c == b':' {
            break;
        }
        if !(c.is_ascii_alphanumeric() || c == b'+' || c == b'-' || c == b'.') {
            return false;
        }
        i += 1;
    }
    s.as_bytes().get(i..i + 3) == Some(b"://")
}

#[cfg(feature = "remote")]
fn strip_file_scheme(s: &str) -> Option<PathBuf> {
    if !s.starts_with("file://") {
        return None;
    }
    let url = url::Url::parse(s).ok()?;
    url.to_file_path().ok()
}

#[cfg(not(feature = "remote"))]
fn strip_file_scheme(s: &str) -> Option<PathBuf> {
    s.strip_prefix("file://").map(PathBuf::from)
}

enum AnyReader {
    Local(istl_core::Reader<istl_core::FileSource>),
    #[cfg(feature = "remote")]
    Remote(istl_core::Reader<istl_objectstore::ObjectStoreSource>),
}

impl AnyReader {
    fn open(endpoint: Endpoint) -> io::Result<Self> {
        match endpoint {
            Endpoint::Local(p) => Ok(AnyReader::Local(istl_core::Reader::open(&p)?)),
            #[cfg(feature = "remote")]
            Endpoint::Remote(url) => {
                let src = istl_objectstore::ObjectStoreSource::open_url(&url)?;
                Ok(AnyReader::Remote(istl_core::Reader::open_source(src)?))
            }
            #[cfg(not(feature = "remote"))]
            Endpoint::Remote(url) => Err(io::Error::other(format!(
                "URL input {url:?} requires the `remote` feature"
            ))),
        }
    }

    fn query_into(
        &self,
        bbox: &istl_core::Bbox,
        format: OutputFormat,
        out: &mut dyn Write,
    ) -> io::Result<QueryStats> {
        match (self, format) {
            (AnyReader::Local(r), OutputFormat::Stl) => r.query_to_writer(bbox, out),
            (AnyReader::Local(r), OutputFormat::Istl) => r.query_to_writer_istl(bbox, out),
            #[cfg(feature = "remote")]
            (AnyReader::Remote(r), OutputFormat::Stl) => r.query_to_writer(bbox, out),
            #[cfg(feature = "remote")]
            (AnyReader::Remote(r), OutputFormat::Istl) => r.query_to_writer_istl(bbox, out),
        }
    }

    /// Read every matched row-group slab into a single contiguous `Vec<u8>` —
    /// the raw 50-byte triangle records concatenated, with no STL header.
    /// Used by `query_numpy` to fill a (N, 3, 3) float32 array.
    fn query_raw_triangles(&self, bbox: &istl_core::Bbox) -> io::Result<(Vec<u8>, u64)> {
        match self {
            AnyReader::Local(r) => read_raw_groups(r, bbox),
            #[cfg(feature = "remote")]
            AnyReader::Remote(r) => read_raw_groups(r, bbox),
        }
    }
}

fn read_raw_groups<R: istl_core::RangeReader>(
    reader: &istl_core::Reader<R>,
    bbox: &istl_core::Bbox,
) -> io::Result<(Vec<u8>, u64)> {
    let matched = reader.matched_groups(bbox);
    let total_bytes: u64 = matched.iter().map(|g| g.byte_size).sum();
    let total_tris: u64 = matched.iter().map(|g| g.triangle_count()).sum();
    let cap: usize = total_bytes
        .try_into()
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "result too large for memory"))?;
    let mut buf: Vec<u8> = Vec::with_capacity(cap);
    let ranges: Vec<(u64, u64)> = matched.iter().map(|g| (g.byte_start, g.byte_size)).collect();
    reader.source().read_to_writer_batch(&ranges, &mut buf)?;
    Ok((buf, total_tris))
}

fn with_output_writer<F>(output: Endpoint, f: F) -> io::Result<QueryStats>
where
    F: FnOnce(&mut dyn Write) -> io::Result<QueryStats>,
{
    match output {
        Endpoint::Local(p) => {
            let f_handle = File::create(&p)?;
            let mut out = BufWriter::with_capacity(8 * 1024 * 1024, f_handle);
            let stats = f(&mut out)?;
            out.flush()?;
            Ok(stats)
        }
        #[cfg(feature = "remote")]
        Endpoint::Remote(url) => {
            let sink = istl_objectstore::ObjectStoreSink::create_url(&url)?;
            let mut buf = BufWriter::with_capacity(8 * 1024 * 1024, sink);
            let stats = f(&mut buf)?;
            buf.flush()?;
            let sink = buf
                .into_inner()
                .map_err(|e| io::Error::other(format!("flushing sink for {url:?}: {e}")))?;
            sink.finish()?;
            Ok(stats)
        }
        #[cfg(not(feature = "remote"))]
        Endpoint::Remote(url) => Err(io::Error::other(format!(
            "URL output {url:?} requires the `remote` feature"
        ))),
    }
}

/// Query an .istl file with a 3D bbox and write the result.
///
/// `input` and `output` may be local paths or URLs. Supported URL schemes
/// (require the `remote` feature, on by default): `s3://`, `gs://`, `az://`,
/// `http(s)://`, `file://`, `memory://`. Credentials come from the standard
/// provider chains (env, profile, IMDS, Lambda role).
///
/// `format` is `"stl"` (default — vanilla binary STL) or `"istl"` (indexed,
/// re-using matched row groups verbatim).
#[pyfunction]
#[pyo3(signature = (input, output, bbox, format = "stl"))]
fn query<'py>(
    py: Python<'py>,
    input: &str,
    output: &str,
    bbox: (f32, f32, f32, f32, f32, f32),
    format: &str,
) -> PyResult<Bound<'py, PyDict>> {
    let q = parse_bbox(bbox)?;
    let fmt = parse_format(format)?;
    let input_endpoint = classify(input);
    let output_endpoint = classify(output);
    let stats = py
        .detach(move || -> io::Result<QueryStats> {
            let reader = AnyReader::open(input_endpoint)?;
            with_output_writer(output_endpoint, |w| reader.query_into(&q, fmt, w))
        })
        .map_err(io_err)?;
    let d = PyDict::new(py);
    d.set_item("groups_matched", stats.groups_matched)?;
    d.set_item("triangles_written", stats.triangles_written)?;
    Ok(d)
}

/// Query an .istl file with a 3D bbox and return the result as in-memory bytes.
///
/// Same input semantics as [`query`] (local path or URL). The returned `bytes`
/// is a complete `.stl` (default) or `.istl` (`format="istl"`) payload,
/// suitable for `trimesh.load(BytesIO(buf), file_type="stl")` or
/// `meshio.read(BytesIO(buf), file_format="stl")`.
///
/// The whole result is held in RAM — fine for AOI-sized queries, expensive
/// for whole-dataset queries. Use [`query`] with a file output for those.
#[pyfunction]
#[pyo3(signature = (input, bbox, format = "stl"))]
fn query_bytes<'py>(
    py: Python<'py>,
    input: &str,
    bbox: (f32, f32, f32, f32, f32, f32),
    format: &str,
) -> PyResult<Bound<'py, PyBytes>> {
    let q = parse_bbox(bbox)?;
    let fmt = parse_format(format)?;
    let input_endpoint = classify(input);
    let buf = py
        .detach(move || -> io::Result<Vec<u8>> {
            let reader = AnyReader::open(input_endpoint)?;
            let mut buf: Vec<u8> = Vec::new();
            reader.query_into(&q, fmt, &mut buf)?;
            Ok(buf)
        })
        .map_err(io_err)?;
    Ok(PyBytes::new(py, &buf))
}

/// Query an .istl file with a 3D bbox and return the result as a NumPy
/// `(N, 3, 3) float32` array of triangle vertices (`array[i, v, :] = (x,y,z)`
/// of vertex `v ∈ {0,1,2}` of triangle `i`).
///
/// Normals and per-triangle attribute bytes are dropped — vertices are what
/// `trimesh.Trimesh(vertices=arr.reshape(-1,3), faces=...)` and most numpy
/// pipelines want. Same superset semantics as [`query`].
#[pyfunction]
#[pyo3(signature = (input, bbox))]
fn query_numpy<'py>(
    py: Python<'py>,
    input: &str,
    bbox: (f32, f32, f32, f32, f32, f32),
) -> PyResult<Bound<'py, PyArray3<f32>>> {
    let q = parse_bbox(bbox)?;
    let input_endpoint = classify(input);
    let (raw, n_tris) = py
        .detach(move || -> io::Result<(Vec<u8>, u64)> {
            let reader = AnyReader::open(input_endpoint)?;
            reader.query_raw_triangles(&q)
        })
        .map_err(io_err)?;

    const TRI_SIZE: usize = 50;
    const VERTS_OFFSET: usize = 12; // skip 12-byte normal
    const VERTS_LEN: usize = 36; // 3 vertices × 3 f32 × 4 bytes

    let n_tris_usize: usize = n_tris
        .try_into()
        .map_err(|_| PyValueError::new_err("result too large to fit in a numpy array"))?;
    if raw.len() != n_tris_usize * TRI_SIZE {
        return Err(PyValueError::new_err(format!(
            "internal: matched {} bytes, expected {} for {} triangles",
            raw.len(),
            n_tris_usize * TRI_SIZE,
            n_tris_usize
        )));
    }

    let arr = PyArray3::<f32>::zeros(py, [n_tris_usize, 3, 3], false);
    // SAFETY: just allocated above, no other reference into it exists.
    {
        let dest: &mut [f32] = unsafe { arr.as_slice_mut()? };
        // dest is 9 f32 per triangle, contiguous (C order). Copy the 36 vertex
        // bytes out of each 50-byte triangle record.
        let dest_bytes: &mut [u8] = unsafe {
            std::slice::from_raw_parts_mut(
                dest.as_mut_ptr() as *mut u8,
                dest.len() * std::mem::size_of::<f32>(),
            )
        };
        for i in 0..n_tris_usize {
            let src_off = i * TRI_SIZE + VERTS_OFFSET;
            let dst_off = i * VERTS_LEN;
            dest_bytes[dst_off..dst_off + VERTS_LEN]
                .copy_from_slice(&raw[src_off..src_off + VERTS_LEN]);
        }
    }
    Ok(arr)
}

#[pyfunction]
fn read_index<'py>(py: Python<'py>, path: &str) -> PyResult<Bound<'py, PyList>> {
    let reader = istl_core::Reader::open(path).map_err(io_err)?;
    let out = PyList::empty(py);
    for g in &reader.index.groups {
        let d = PyDict::new(py);
        d.set_item("byte_start", g.byte_start)?;
        d.set_item("byte_size", g.byte_size)?;
        d.set_item("xmin", g.bbox.xmin)?;
        d.set_item("xmax", g.bbox.xmax)?;
        d.set_item("ymin", g.bbox.ymin)?;
        d.set_item("ymax", g.bbox.ymax)?;
        d.set_item("zmin", g.bbox.zmin)?;
        d.set_item("zmax", g.bbox.zmax)?;
        out.append(d)?;
    }
    Ok(out)
}

#[pymodule]
fn istl(m: &Bound<'_, PyModule>) -> PyResult<()> {
    m.add_function(wrap_pyfunction!(convert, m)?)?;
    m.add_function(wrap_pyfunction!(query, m)?)?;
    m.add_function(wrap_pyfunction!(query_bytes, m)?)?;
    m.add_function(wrap_pyfunction!(query_numpy, m)?)?;
    m.add_function(wrap_pyfunction!(read_index, m)?)?;
    m.add("DEFAULT_GROUP_SIZE", istl_core::DEFAULT_GROUP_SIZE)?;
    Ok(())
}
