// PyO3 extension module "fastmath": registers compute; helper is internal and
// must not be reachable from the Python side.
use pyo3::prelude::*;

#[pyfunction]
fn compute(x: i64) -> i64 {
    helper(x) * 2
}

fn helper(x: i64) -> i64 {
    x + 1
}

#[pymodule]
fn fastmath(_py: Python<'_>, m: &PyModule) -> PyResult<()> {
    m.add_function(wrap_pyfunction!(compute, m)?)?;
    Ok(())
}
