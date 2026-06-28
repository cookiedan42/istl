use std::fs::File;
use std::io::{self, BufWriter, Write};
use std::path::PathBuf;

use pyo3::exceptions::{PyIOError, PyValueError};
use pyo3::prelude::*;
use pyo3::types::{PyDict, PyList};

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
    let (xmin, ymin, zmin, xmax, ymax, zmax) = bbox;
    if !(xmin <= xmax && ymin <= ymax && zmin <= zmax) {
        return Err(PyValueError::new_err(
            "bbox has min > max on at least one axis",
        ));
    }
    let q = istl_core::Bbox {
        xmin,
        xmax,
        ymin,
        ymax,
        zmin,
        zmax,
    };
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
    m.add_function(wrap_pyfunction!(read_index, m)?)?;
    m.add("DEFAULT_GROUP_SIZE", istl_core::DEFAULT_GROUP_SIZE)?;
    Ok(())
}
