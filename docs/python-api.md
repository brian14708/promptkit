# Python Host API

The `isola` package exposes an async host-side API for compiling sandbox
templates and running code inside isolated runtimes.

This page documents the embedding SDK that runs in your host process. For the
Python modules available inside sandboxed guest code, see
[Python Guest API](python-guest-api.md). For the JavaScript globals available
inside JS guests, see [JavaScript Guest API](javascript-guest-api.md).

## Install

```bash
pip install isola
```

## Lifecycle

The normal flow is:

1. `await build_template(...)`
2. `async with template.create(...) as sandbox:`
3. `await sandbox.load_script(...)`
4. `await sandbox.run(...)` or iterate `sandbox.run_stream(...)`

```python
import asyncio

from isola import build_template


async def main() -> None:
    template = await build_template("python")
    async with template.create() as sandbox:
        await sandbox.load_script("def hello(name):\n    return f'hello {name}'")
        result = await sandbox.run("hello", "world")
        print(result)


asyncio.run(main())
```

## Runtime Resolution

Most users can skip this section and let `build_template(...)` handle runtime
downloads automatically.

Use `resolve_runtime(runtime, *, version=None)` to fetch the runtime bundle
config directly:

```python
from isola import resolve_runtime

config = await resolve_runtime("python")
```

When `runtime_path` is omitted from `build_template(...)`, the SDK resolves the
runtime automatically, downloads the matching release asset on first use,
verifies its digest, and caches it under `~/.cache/isola/runtimes/`.

Supported runtime names:

- `"python"`
- `"js"`

## Core Types

### `build_template(...)`

Builds and returns a reusable sandbox template using an internal `SandboxContext`.

```python
from isola import build_template

template = await build_template(runtime, *, version=None, **template_config)
```

`build_template(...)` accepts:

- `runtime`: `"python"` or `"js"`
- `version`: optional release tag to resolve when auto-downloading a runtime
- `runtime_path`: directory or path used to initialize the runtime bundle
- `runtime_lib_dir`: runtime library directory, required for Python runtimes that are provided manually
- `cache_dir`: template cache directory
- `max_memory`: template memory limit in bytes
- `prelude`: code injected before user scripts
- `mounts`: `list[MountConfig]`
- `env`: `dict[str, str]`

### `SandboxContext`

Advanced API for explicitly owning a template compilation context. It exposes
the same template-building behavior as the top-level helper, plus explicit
`close()` and async context-manager lifecycle control.

`SandboxContext` supports `async with ...` cleanup and explicit `close()`.

### `SandboxTemplate`

Instantiates sandboxes from a compiled template.

```python
async with template.create(**sandbox_config) as sandbox:
    ...
```

Use `create(...)` for the normal case where the sandbox lifetime is scoped to an
async context manager. If you need the raw `Sandbox` object before entering it,
use `await template.instantiate(**sandbox_config)` instead.

`create(...)` and `instantiate(...)` accept:

- `max_memory`: per-sandbox memory limit in bytes
- `mounts`: `list[MountConfig]`
- `env`: `dict[str, str]`
- `hostcalls`: `dict[str, async callable]` used for guest `sandbox.asyncio.hostcall(...)`
- `http`: `None` to disable guest HTTP, `True` to use the built-in `httpx2`
  bridge, or an async callable for a custom outbound HTTP policy

### `Sandbox`

Runs guest code inside an instantiated sandbox.

```python
async with sandbox:
    await sandbox.load_script(code)
    result = await sandbox.run(name, *args, **kwargs)
```

Public methods:

- `await load_script(code)`
- `await run(name, *args, **kwargs) -> JsonValue | None`
- `run_stream(name, *args, **kwargs) -> AsyncIterator[Event]`
- `close()`
- `await aclose()`

Use `async with sandbox:` before executing code. Entering the async context starts the sandbox.

## Arguments and Streaming

`run(...)` and `run_stream(...)` accept positional JSON values directly:

```python
result = await sandbox.run("add", 1, 2)
```

Keyword arguments become named guest arguments:

```python
result = await sandbox.run("add", a=1, b=2)
```

Use `Arg(value, name="...")` to pass a named argument:

```python
from isola import Arg

result = await sandbox.run("add", Arg(2, name="b"), Arg(1, name="a"))
```

Use `StreamArg` for JSON streams passed into guest code:

```python
from isola import StreamArg

stream = StreamArg.from_iterable([1, 2, 3])
result = await sandbox.run("consume", stream)
```

JSON lists are passed as normal values:

```python
result = await sandbox.run("consume_list", [1, 2, 3])
```

Available constructors:

- `StreamArg.from_iterable(values, *, name=None, capacity=1024)`
- `StreamArg.from_async_iterable(values, *, name=None, capacity=1024)`

`Arg` and the public name on `StreamArg` are immutable. Passing either through
`**kwargs` creates a named wrapper without modifying the caller's object.

## Hostcalls

Register host callbacks when the sandbox is created. Each handler receives the
decoded payload for its call name and must return a serializable value.

Guest sandboxes invoke these handlers with their runtime-specific guest APIs:

- Python guests use `sandbox.asyncio.hostcall(...)`
- JS guests use top-level `await hostcall(...)`

Those guest-side call sites are documented separately in
[Python Guest API](python-guest-api.md) and
[JavaScript Guest API](javascript-guest-api.md).

```python
from sandbox.asyncio import hostcall


async def lookup_user(payload: dict[str, object]) -> object:
    user_id = int(payload["user_id"])
    return {"user_id": user_id, "name": f"user-{user_id}"}


async with template.create(hostcalls={"lookup_user": lookup_user}) as sandbox:
    await sandbox.load_script(
        "from sandbox.asyncio import hostcall\n"
        "\n"
        "async def lookup_user(user_id):\n"
        "    return await hostcall('lookup_user', {'user_id': user_id})\n"
    )
    result = await sandbox.run("lookup_user", 7)
```

Configure hostcalls and HTTP behavior when the sandbox is created.

## Events and Results

`run(...)` returns the final value directly:

```python
result = await sandbox.run("add", 1, 2)
assert result == 3
```

`run_stream(...)` yields typed `Event` objects for fine-grained control:

```python
async for event in sandbox.run_stream("compute"):
    match event:
        case ResultEvent(data=value):
            print("intermediate:", value)
        case EndEvent(data=value):
            print("final:", value)
        case StdoutEvent(data=line):
            print("stdout:", line)
        case StderrEvent(data=line):
            print("stderr:", line)
        case ErrorEvent(data=msg):
            print("error:", msg)
        case LogEvent(data=msg):
            print("log:", msg)
```

`Event` is a union of: `ResultEvent`, `EndEvent`, `StdoutEvent`, `StderrEvent`, `ErrorEvent`, `LogEvent`. Each carries a typed `data` field (`JsonValue` for result/end, `str` for the rest).

## Filesystem and Environment

Use `MountConfig` to mount host paths into the guest:

```python
from isola import MountConfig

mount = MountConfig(
    host="./data",
    guest="/workspace",
    dir_perms="read",
    file_perms="read",
)
```

`dir_perms` and `file_perms` accept:

- `"read"`
- `"write"`
- `"read-write"`

Environment variables can be supplied in both template and sandbox config via `env={"KEY": "value"}`.

## HTTP Bridge

Outbound guest HTTP is disabled unless you pass `http=` when creating
the sandbox. Use `http=True` to enable the built-in `httpx2` pass-through
bridge, or provide your own async handler to enforce a custom HTTP policy.

The guest-side request APIs used inside the sandbox are documented in
[Python Guest API](python-guest-api.md) and
[JavaScript Guest API](javascript-guest-api.md).

```python
from collections.abc import AsyncIterator

from isola import HttpRequest, HttpResponse


async def body() -> AsyncIterator[bytes]:
    yield b"hello "
    yield b"world"


async def http(request: HttpRequest) -> HttpResponse:
    return HttpResponse(
        status=200,
        headers={"content-type": "text/plain"},
        body=body(),
    )


async with template.create(http=http) as sandbox:
    ...
```

Request and response models:

- `HttpRequest(method, url, headers, body)`
- `HttpResponse(status, headers={}, body=None)`

`HttpResponse.body` may be:

- `bytes`
- `AsyncIterable[bytes]`
- `None`

## Errors

The package exports these exception types:

- `IsolaError`
- `InvalidArgumentError`
- `InternalError`
- `StreamFullError`
- `StreamClosedError`
