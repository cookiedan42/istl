use pyo3::exceptions::{PyIOError, PyValueError};
use pyo3::prelude::*;
use pyo3::types::{PyDict, PyList};

fn io_err<E: std::fmt::Display>(e: E) -> PyErr {
    PyIOError::new_err(e.to_string())
}

#[pyfunction]
#[pyo3(signature = (input, output, group_size=8192))]
fn convert<'py>(
    py: Python<'py>,
    input: &str,
    output: &str,
    group_size: usize,
) -> PyResult<Bound<'py, PyDict>> {
    let stats = py
        .detach(|| istl_core::convert(input, output, group_size))
        .map_err(io_err)?;
    let d = PyDict::new(py);
    d.set_item("triangles", stats.triangles)?;
    d.set_item("groups", stats.groups)?;
    d.set_item("footer_offset", stats.footer_offset)?;
    d.set_item("footer_size", stats.footer_size)?;
    d.set_item(
        "dataset_bbox",
        (
            stats.dataset_bbox[0],
            stats.dataset_bbox[1],
            stats.dataset_bbox[2],
            stats.dataset_bbox[3],
            stats.dataset_bbox[4],
            stats.dataset_bbox[5],
        ),
    )?;
    Ok(d)
}

#[pyfunction]
#[pyo3(signature = (input, output, bbox))]
fn query<'py>(
    py: Python<'py>,
    input: &str,
    output: &str,
    bbox: (f32, f32, f32, f32, f32, f32),
) -> PyResult<Bound<'py, PyDict>> {
    let (xmin, ymin, zmin, xmax, ymax, zmax) = bbox;
    if !(xmin <= xmax && ymin <= ymax && zmin <= zmax) {
        return Err(PyValueError::new_err(
            "bbox has min > max on at least one axis",
        ));
    }
    let q = [xmin, xmax, ymin, ymax, zmin, zmax];
    let stats = py
        .detach(|| {
            let reader = istl_core::Reader::open(input)?;
            reader.query(&q, output)
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
        d.set_item("xmin", g.bbox[0])?;
        d.set_item("xmax", g.bbox[1])?;
        d.set_item("ymin", g.bbox[2])?;
        d.set_item("ymax", g.bbox[3])?;
        d.set_item("zmin", g.bbox[4])?;
        d.set_item("zmax", g.bbox[5])?;
        out.append(d)?;
    }
    Ok(out)
}

#[pymodule]
fn istl(m: &Bound<'_, PyModule>) -> PyResult<()> {
    m.add_function(wrap_pyfunction!(convert, m)?)?;
    m.add_function(wrap_pyfunction!(query, m)?)?;
    m.add_function(wrap_pyfunction!(read_index, m)?)?;
    Ok(())
}
