#[pyo3::pymodule]
#[pyo3(name = "_isola_http")]
pub mod http_module {
    use isola_runtime::wasi_http::{HttpRequest, HttpResponse};
    use pyo3::{
        prelude::*,
        types::{PyBytes, PyDict},
    };
    use url::Url;

    use crate::{
        serde::python_to_json_writer,
        wasm::{
            PyPollable,
            body_buffer::{BodyBuffer, Buffer},
            future::create_future,
        },
    };

    #[pyfunction]
    fn new_buffer(kind: &str) -> PyResult<ResponseBuffer> {
        let inner = Buffer::new(kind).ok_or_else(|| {
            pyo3::exceptions::PyValueError::new_err(format!("invalid buffer kind: {kind}"))
        })?;
        Ok(ResponseBuffer { inner })
    }

    #[pyfunction]
    #[pyo3(signature = (method, url, params, headers, body, timeout))]
    fn fetch(
        method: &str,
        url: &str,
        params: Option<&Bound<'_, PyDict>>,
        headers: Option<&Bound<'_, PyDict>>,
        body: Option<&Bound<'_, PyAny>>,
        timeout: Option<f64>,
    ) -> PyResult<PyFutureResponse> {
        enum Body<'a> {
            None,
            Bytes(Bound<'a, PyBytes>),
            Object(Bound<'a, PyAny>),
        }

        let body = body.map_or(Body::None, |body| {
            body.extract::<Bound<'_, PyBytes>>()
                .map_or(Body::Object(body.clone()), Body::Bytes)
        });

        let mut header_fields = Vec::new();
        if let Some(headers) = headers {
            for (k, v) in headers {
                let k: String = k.extract()?;
                let v: &str = v.extract()?;
                header_fields.push((k, v.as_bytes().to_vec()));
            }
        }
        if matches!(body, Body::Object(_))
            && !header_fields
                .iter()
                .any(|(name, _)| name.eq_ignore_ascii_case("content-type"))
        {
            header_fields.push(("content-type".to_string(), b"application/json".to_vec()));
        }

        let mut u = Url::parse(url)
            .map_err(|e| PyErr::new::<pyo3::exceptions::PyTypeError, _>(e.to_string()))?;
        if let Some(params) = params {
            for (k, v) in params {
                u.query_pairs_mut().append_pair(k.extract()?, v.extract()?);
            }
        }
        let timeout_ms = timeout
            .filter(|timeout| timeout.is_finite() && *timeout > 0.0)
            .map(|timeout| std::time::Duration::from_secs_f64(timeout).as_millis())
            .map(|timeout_ms| u64::try_from(timeout_ms).unwrap_or(u64::MAX));

        let body = match &body {
            Body::None => None,
            Body::Bytes(b) => Some(b.as_bytes().to_vec()),
            Body::Object(b) => {
                let mut bytes = Vec::new();
                python_to_json_writer(b.clone(), &mut bytes)
                    .map_err(|_| PyErr::new::<pyo3::exceptions::PyTypeError, _>("serde error"))?;
                Some(bytes)
            }
        };

        Ok(PyFutureResponse::new(crate::wasm::future::register_http(
            HttpRequest::new(method.to_string(), u, header_fields, body, timeout_ms),
        )))
    }

    create_future!(PyFutureResponse, http -> PyResponse);

    #[pyclass]
    struct PyResponse {
        status: u16,
        headers: Vec<(String, Vec<u8>)>,
        body: Vec<u8>,
        cursor: usize,
        consumed: bool,
        closed: bool,
    }

    impl TryFrom<Result<HttpResponse, String>> for PyResponse {
        type Error = PyErr;

        fn try_from(value: Result<HttpResponse, String>) -> Result<Self, Self::Error> {
            match value {
                Ok(response) => Ok(Self {
                    status: response.status,
                    headers: response.headers,
                    body: response.body,
                    cursor: 0,
                    consumed: false,
                    closed: false,
                }),
                Err(e) => Err(PyErr::new::<pyo3::exceptions::PyTypeError, _>(e)),
            }
        }
    }

    #[pymethods]
    impl PyResponse {
        const fn close(&mut self) {
            self.closed = true;
        }

        fn status(&self) -> PyResult<u16> {
            if self.closed {
                return Err(PyErr::new::<pyo3::exceptions::PyRuntimeError, _>(
                    "response closed",
                ));
            }
            Ok(self.status)
        }

        fn headers<'py>(slf: &Bound<'py, Self>) -> PyResult<Bound<'py, PyDict>> {
            let borrowed = slf.borrow();
            if borrowed.closed {
                return Err(PyErr::new::<pyo3::exceptions::PyRuntimeError, _>(
                    "response closed",
                ));
            }
            let d = PyDict::new(slf.py());
            for (k, v) in &borrowed.headers {
                d.set_item(
                    k,
                    std::str::from_utf8(v).map_err(|_| {
                        PyErr::new::<pyo3::exceptions::PyTypeError, _>("invalid header value")
                    })?,
                )?;
            }
            Ok(d)
        }

        fn read_into(
            &mut self,
            buf: &mut ResponseBuffer,
            size: i64,
        ) -> PyResult<Option<PyPollable>> {
            read_into(self, &mut buf.inner, size)
        }

        fn blocking_read<'py>(
            &mut self,
            py: Python<'py>,
            kind: &str,
            size: i64,
        ) -> PyResult<Option<Bound<'py, PyAny>>> {
            if self.consumed {
                return Err(PyErr::new::<pyo3::exceptions::PyRuntimeError, _>(
                    "Response already read",
                ));
            }
            let mut buf = Buffer::new(kind).ok_or_else(|| {
                pyo3::exceptions::PyValueError::new_err(format!("invalid buffer kind: {kind}"))
            })?;
            while read_into(self, &mut buf, size)?.is_some() {}
            if size < 0 {
                self.consumed = true;
            }
            buf.decode_all(py)
        }
    }

    impl Drop for PyResponse {
        fn drop(&mut self) {
            self.close();
        }
    }

    /// Decode a CBOR-transported byte array (serialized as a sequence of
    /// integers) back into raw bytes.
    fn read_into(
        slf: &mut PyResponse,
        buf: &mut impl BodyBuffer,
        size: i64,
    ) -> PyResult<Option<PyPollable>> {
        if slf.closed {
            return Err(PyErr::new::<pyo3::exceptions::PyRuntimeError, _>(
                "response closed",
            ));
        }
        if slf.consumed {
            return Err(PyErr::new::<pyo3::exceptions::PyRuntimeError, _>(
                "Response already read",
            ));
        }
        let read_size = if size < 0 {
            slf.body.len().saturating_sub(slf.cursor)
        } else {
            usize::try_from(size).map_err(|_| {
                PyErr::new::<pyo3::exceptions::PyOverflowError, _>(
                    "read size is too large for this platform",
                )
            })?
        };
        // A zero-length read makes no progress. Report completion immediately so
        // callers that loop until `None` (e.g. `blocking_read`, `_aread`) don't
        // spin forever on the always-ready pollable. The response is left
        // unconsumed so subsequent reads still work.
        if read_size == 0 && slf.cursor < slf.body.len() {
            return Ok(None);
        }
        let end = slf.cursor.saturating_add(read_size).min(slf.body.len());
        if slf.cursor < end {
            buf.write(&slf.body[slf.cursor..end]);
            slf.cursor = end;
        }
        if slf.cursor >= slf.body.len() {
            buf.close();
            slf.consumed = true;
            Ok(None)
        } else {
            Ok(Some(PyPollable::default()))
        }
    }

    #[pyclass]
    struct ResponseBuffer {
        inner: Buffer,
    }

    #[pymethods]
    impl ResponseBuffer {
        fn next(&mut self, py: Python<'_>) -> PyResult<Option<Py<PyAny>>> {
            self.inner.decode(py).map(|o| o.map(Into::into))
        }

        fn read_all(&mut self, py: Python<'_>) -> PyResult<Option<Py<PyAny>>> {
            self.inner.decode_all(py).map(|o| o.map(Into::into))
        }
    }
}
