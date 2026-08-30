use std::borrow::Cow;

use pyo3::{
    PyTypeInfo,
    exceptions::{PyNameError, PyValueError},
    intern,
    prelude::*,
    sync::PyOnceLock,
    types::{
        PyBool, PyByteArray, PyBytes, PyDict, PyFloat, PyInt, PyList, PyMemoryView, PyString,
        PyTuple,
    },
};

use crate::{
    error::{Error, Result},
    pymeta,
    serde::{cbor_to_python, python_to_cbor_emit},
    wasm::{ArgIter, isola::script::host::EmitType},
};

pub struct Scope {
    locals: Py<PyAny>,
    stdio: Option<(Py<PyAny>, Py<PyAny>)>,
}

pub enum InputValue<'a> {
    Cbor(Cow<'a, [u8]>),
    Iter(ArgIter),
}

impl Scope {
    pub fn new() -> Self {
        Python::initialize();
        Python::attach(|py| {
            let locals = PyDict::new(py);
            locals
                .set_item(
                    "__builtins__",
                    PyModule::import(py, intern!(py, "builtins")).unwrap(),
                )
                .unwrap();

            let stdio = PyModule::import(py, intern!(py, "sys"))
                .ok()
                .and_then(|sys| {
                    if let Ok(path) = sys.getattr(intern!(py, "path")) {
                        let path = path.cast_exact::<PyList>().ok();
                        if let Some(path) = path {
                            let _ = path.insert(1, "/lib/bundle.zip");
                        }
                    }
                    #[cfg(target_os = "wasi")]
                    {
                        PyModule::import(py, intern!(py, "sandbox._httpx2"))
                            .expect("failed to import HTTPX2 sandbox transport")
                            .getattr(intern!(py, "install"))
                            .expect("failed to find HTTPX2 sandbox transport installer")
                            .call0()
                            .expect("failed to install HTTPX2 sandbox transport");
                    }
                    match (
                        sys.getattr(intern!(py, "stdout")).ok(),
                        sys.getattr(intern!(py, "stderr")).ok(),
                    ) {
                        (Some(stdout), Some(stderr)) => Some((
                            stdout.into_pyobject(py).unwrap().into(),
                            stderr.into_pyobject(py).unwrap().into(),
                        )),
                        _ => None,
                    }
                });

            Self {
                locals: locals.into_pyobject(py).unwrap().into(),
                stdio,
            }
        })
    }

    pub fn flush(&self) {
        let _ = Python::attach(|py| {
            if let Some((stdout, stderr)) = &self.stdio {
                let flush = intern!(py, "flush");
                stdout.call_method0(py, flush)?;
                stderr.call_method0(py, flush)?;
            }
            Ok::<_, PyErr>(())
        });
    }

    pub fn load_script(&self, code: &str) -> crate::error::Result<()> {
        static INIT: PyOnceLock<Py<PyAny>> = PyOnceLock::new();

        Python::attach(|py| {
            if let Some(meta) = pymeta::parse_pep723(code) {
                INIT.import(py, "sandbox.importlib", "_initialize_pep723")
                    .expect("failed to import sandbox.importlib")
                    .call1((meta,))
                    .map_err(|e| Error::from_pyerr(py, e))?;
            }
            let code = std::ffi::CString::new(code).map_err(|e| {
                Error::from_pyerr(
                    py,
                    PyValueError::new_err(format!("script contains NUL byte: {e}")),
                )
            })?;
            py.run(
                &code,
                Some(
                    self.locals
                        .cast_bound(py)
                        .map_err(|e| Error::from_pyerr(py, e))?,
                ),
                None,
            )
            .map_err(|e| Error::from_pyerr(py, e))?;
            Ok(())
        })
    }

    fn is_serializable(pyobject: &Bound<'_, PyAny>) -> bool {
        pyobject.is_none()
            || PyDict::is_exact_type_of(pyobject)
            || PyList::is_exact_type_of(pyobject)
            || PyTuple::is_exact_type_of(pyobject)
            || PyString::is_exact_type_of(pyobject)
            || PyBool::is_exact_type_of(pyobject)
            || PyInt::is_exact_type_of(pyobject)
            || PyFloat::is_exact_type_of(pyobject)
            || PyBytes::is_exact_type_of(pyobject)
            || PyByteArray::is_exact_type_of(pyobject)
            || PyMemoryView::is_exact_type_of(pyobject)
            || (pyobject
                .get_type()
                .module()
                .is_ok_and(|module| module.to_str().is_ok_and(|module| module == "numpy"))
                && pyobject.hasattr("dtype").unwrap_or_default())
    }

    pub fn run<'a, U>(
        &self,
        name: &str,
        positional: impl IntoIterator<Item = InputValue<'a>, IntoIter = U>,
        named: impl IntoIterator<Item = (Cow<'a, str>, InputValue<'a>)>,
        mut callback: impl FnMut(crate::wasm::isola::script::host::EmitType, &[u8]),
    ) -> Result<()>
    where
        U: ExactSizeIterator<Item = InputValue<'a>>,
    {
        Python::attach(|py| {
            let dict: &Bound<'_, PyDict> = self
                .locals
                .cast_bound(py)
                .map_err(|e| Error::from_pyerr(py, e))?;
            let Some(f) = dict.get_item(name).map_err(|e| Error::from_pyerr(py, e))? else {
                return Err(Error::from_pyerr(
                    py,
                    PyNameError::new_err(format!("name '{name}' is not defined")),
                ));
            };

            let obj = if f.is_callable() {
                let args = PyTuple::new(
                    py,
                    positional
                        .into_iter()
                        .map(|v| match v {
                            InputValue::Iter(it) => Ok(it.into_pyobject(py).unwrap().into_any()),
                            InputValue::Cbor(v) => Ok(cbor_to_python(py, v.as_ref())
                                .map_err(|e| Error::from_pyerr(py, e))?),
                        })
                        .collect::<Result<Vec<_>>>()?,
                )
                .map_err(|_e| Error::UnexpectedError("Failed to create Python tuple"))?;
                let kwargs = PyDict::new(py);
                for (k, v) in named {
                    match v {
                        InputValue::Cbor(v) => {
                            kwargs
                                .set_item(
                                    k,
                                    cbor_to_python(py, v.as_ref())
                                        .map_err(|e| Error::from_pyerr(py, e))?,
                                )
                                .map_err(|e| Error::from_pyerr(py, e))?;
                        }
                        InputValue::Iter(it) => kwargs
                            .set_item(k, it.into_pyobject(py).unwrap())
                            .map_err(|e| Error::from_pyerr(py, e))?,
                    }
                }
                f.as_borrowed()
                    .call(args, Some(&kwargs))
                    .map_err(|e| Error::from_pyerr(py, e))?
            } else {
                f
            };

            let obj = if obj.hasattr("__await__").unwrap_or_default()
                || obj.hasattr("__aiter__").unwrap_or_default()
            {
                static ASYNC_RUN: PyOnceLock<Py<PyAny>> = PyOnceLock::new();
                ASYNC_RUN
                    .import(py, "sandbox.asyncio", "run")
                    .expect("failed to import sandbox.asyncio")
                    .call1((obj,))
                    .map_err(|e| Error::from_pyerr(py, e))?
            } else {
                obj
            };

            if Self::is_serializable(&obj) {
                return python_to_cbor_emit(obj, EmitType::End, callback)
                    .map_err(|e| Error::from_pyerr(py, e));
            }

            if let Ok(iter) = obj.try_iter() {
                for el in iter {
                    python_to_cbor_emit(
                        el.map_err(|e| Error::from_pyerr(py, e))?,
                        EmitType::PartialResult,
                        &mut callback,
                    )
                    .map_err(|e| Error::from_pyerr(py, e))?;
                }

                callback(EmitType::End, &[]);
                return Ok(());
            }

            Err(Error::UnexpectedError(
                "Return type is not serializable or iterable",
            ))
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn numpy_arrays_are_serializable_results() {
        Python::initialize();
        Python::attach(|py| {
            let Ok(np) = py.import("numpy") else { return };
            let array = np.call_method1("array", (vec![1.5, -2.25], "<f4")).unwrap();
            assert!(Scope::is_serializable(&array));
        });
    }

    #[test]
    fn test_python_to_cbor_emit() {
        use std::cell::RefCell;
        let emissions = RefCell::new(Vec::new());

        Python::initialize();

        {
            let emit_fn = |emit_type: crate::wasm::isola::script::host::EmitType, data: &[u8]| {
                emissions.borrow_mut().push((emit_type, data.to_vec()));
            };

            // Test the python_to_cbor_emit function with a simple value
            pyo3::Python::attach(|py| {
                let test_string = pyo3::types::PyString::new(py, "test");
                let test_value = test_string.as_any();
                python_to_cbor_emit(test_value.clone(), EmitType::End, emit_fn).unwrap();
            });
        }

        // Check that emission occurred
        let emissions = emissions.into_inner();
        assert_eq!(emissions.len(), 1);
        assert_eq!(emissions[0].0, EmitType::End);
        // The exact content will be CBOR-encoded "test"
        assert!(!emissions[0].1.is_empty());
    }

    #[test]
    fn failed_python_to_cbor_emit_does_not_finalize() {
        let mut emissions = Vec::new();

        Python::initialize();
        Python::attach(|py| {
            let long_string = PyString::new(py, &"x".repeat(128 * 1024)).into_any();
            let unsupported = py
                .import("builtins")
                .unwrap()
                .getattr("object")
                .unwrap()
                .call0()
                .unwrap();
            let value = PyList::new(py, [long_string, unsupported])
                .unwrap()
                .into_any();

            let result = python_to_cbor_emit(value, EmitType::End, |emit_type, bytes| {
                emissions.push((emit_type, bytes.to_vec()));
            });
            assert!(result.is_err());
        });

        assert!(!emissions.is_empty(), "expected at least one full chunk");
        assert_eq!(emissions.last(), Some(&(EmitType::Abort, Vec::new())));
        assert!(
            emissions[..emissions.len() - 1]
                .iter()
                .all(|(emit_type, _)| *emit_type == EmitType::Continuation)
        );
    }

    #[test]
    fn test() {
        let content = r#"
i = 1
def hello(n):
    n += i
    return f"hello {n}"

def sum(i):
    total = 0
    for x in i:
        total += x
    return total
i += 21

def gen():
    for i in range(10):
        yield i
"#;
        let s = Scope::new();
        s.load_script(content).unwrap();
        let mut x = vec![];
        s.run(
            "hello",
            [InputValue::Cbor(minicbor_serde::to_vec(32).unwrap().into())],
            [],
            |_emit_type, data| {
                x.push(data.to_owned());
            },
        )
        .unwrap();
        assert_eq!(x[0], minicbor_serde::to_vec("hello 54").unwrap());

        let mut x = vec![];
        s.run("i", [], [], |_emit_type, data| {
            x.push(data.to_owned());
        })
        .unwrap();
        assert_eq!(x[0], minicbor_serde::to_vec(22).unwrap());

        let mut v = vec![];
        s.run("gen", [], [], |emit_type, data| {
            if emit_type == EmitType::PartialResult {
                v.push(data.to_owned());
            }
        })
        .unwrap();
        assert_eq!(v.len(), 10);
        for (i, vv) in v.iter().enumerate() {
            assert_eq!(*vv, minicbor_serde::to_vec(i).unwrap());
        }
    }
}
