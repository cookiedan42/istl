use super::io_err;
use pyo3::prelude::*;
use pyo3::types::PyDict;

/// Convert a plain binary STL to a Hilbert-sorted indexed .istl file.
#[pyfunction]
#[pyo3(signature = (input, output, group_size=istl_core::DEFAULT_GROUP_SIZE))]
pub fn convert<'py>(
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
    let b = stats.dataset_bbox;
    d.set_item(
        "dataset_bbox",
        (b.xmin, b.xmax, b.ymin, b.ymax, b.zmin, b.zmax),
    )?;
    Ok(d)
}
