# Python Guest API

The `sandbox` package is available to code running inside a Python guest
runtime.

This page documents guest-side APIs. For the host-side embedding SDK that
builds templates and starts sandboxes, see [Python Host API](python-api.md).

## Execution Model

Guest entrypoints can be regular functions or `async def` coroutines:

```python
from sandbox.asyncio import hostcall


def add(a, b):
    return a + b


async def lookup_user(user_id):
    return await hostcall("lookup_user", {"user_id": user_id})
```

If a guest function returns an iterable or async iterable, each yielded item is
emitted to the host as a partial result and the final result is `None`:

```python
def stream_values():
    for i in range(3):
        yield i
```

Values crossing the host/guest boundary should be JSON-like unless you are
working with raw HTTP bodies.

## `sandbox.asyncio`

Import async helpers with:

```python
from sandbox.asyncio import hostcall, run, subscribe
```

### `await hostcall(call_type, payload) -> object`

Calls a host-registered callback and resolves to the returned value.

```python
from sandbox.asyncio import hostcall


async def main(user_id):
    return await hostcall("lookup_user", {"user_id": user_id})
```

### `run(main)`

Runs a coroutine or async generator on the guest poll loop from synchronous
guest code.

Use this when you need to drive async work yourself from a synchronous helper or
module initialization path. When the host directly invokes an `async def`
function, Isola awaits it automatically and you do not need `run(...)`.

```python
from sandbox.asyncio import hostcall, run


async def fetch_user(user_id):
    return await hostcall("lookup_user", {"user_id": user_id})


def main(user_id):
    return run(fetch_user(user_id))
```

### `await subscribe(pollable) -> object`

Low-level helper for awaiting native guest pollables. Most guest code should use
`hostcall(...)` or `httpx2.AsyncClient` instead of calling `subscribe(...)`
directly.

## HTTPX2

[HTTPX2](https://httpx2.pydantic.dev/)'s request and response API is available
in the guest:

```python
import httpx2
```

Guest HTTP is only available when the host enables outbound requests with
`http=`.
See [Python Host API](python-api.md) and
[Node.js Host API](nodejs-api.md).

Isola patches HTTPX2's default sync and async transports so requests use the
sandbox HTTP hostcall. HTTPX2 still handles request construction, query
parameters, JSON and form encoding, multipart uploads, response decoding,
redirects, cookies, and status errors using its normal API. Both top-level
helpers such as `httpx2.get(...)` and reusable clients work.

### Synchronous usage

```python
import httpx2


def main(url):
    resp = httpx2.get(url, params={"q": "hello"})
    return {
        "status": resp.status_code,
        "headers": dict(resp.headers),
        "body": resp.text,
    }
```

Use `httpx2.Client` for cookies, redirects, shared headers, and multiple
requests. Streaming responses are available through `httpx2.stream(...)` or
`Client.stream(...)`.

### Asynchronous usage

```python
import httpx2


async def main(url):
    async with httpx2.AsyncClient() as client:
        resp = await client.get(url)
        return resp.text
```

The host owns the network connection, so transport-level HTTPX2 options such as
TLS verification, certificates, connection limits, retries, HTTP version
selection, Unix sockets, and socket options do not apply. A request timeout is
forwarded to the host bridge as the exchange timeout.

A proxy is an egress concern and the guest has no sockets, so proxy selection is
host policy, not guest configuration: configure it once on the host (for example
via the handler's HTTP client or `HTTPS_PROXY`) and it applies to every request.

An explicit custom `transport=` still takes precedence and bypasses the Isola
HTTP bridge. Guest HTTP remains subject to the host's configured policy and
response-size limits.

## `sandbox.importlib`

Import remote modules over HTTP with:

```python
from sandbox.importlib import http
```

`http(url)` returns a context manager that temporarily adds an importer to
`sys.meta_path`:

```python
from sandbox.importlib import http


def main():
    with http("https://example.com/modules"):
        import helpers

    return helpers.answer()
```

The URL may point at a module tree or a zip archive. This importer is also used
internally for Isola's URL-based dependency loading.

## `sandbox.logging`

Import structured log helpers with:

```python
from sandbox.logging import debug, error, info, warning
```

These emit guest log events back to the host sink. `print(...)` still writes to
stdout.

## `sandbox.serde`

Import serialization helpers with:

```python
from sandbox.serde import dumps, loads
```

Supported formats are `"json"`, `"yaml"`, and `"cbor"`:

```python
from sandbox.serde import dumps, loads


payload = dumps({"hello": "world"}, "json")
value = loads(payload, "json")
```

`dumps(value, format)` returns a `str` for JSON/YAML and `bytes` for CBOR.
`loads(value, format)` performs the reverse conversion.
