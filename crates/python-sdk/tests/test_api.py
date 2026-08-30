from __future__ import annotations

import asyncio
import io
import os
import tarfile
from pathlib import Path
from types import SimpleNamespace
from typing import TYPE_CHECKING, Any, cast

import pytest

if TYPE_CHECKING:
    from collections.abc import AsyncIterator, Awaitable, Callable

    from isola import HttpRequest, HttpResponse
    from isola import Sandbox as IsolaSandbox

isola = pytest.importorskip("isola")
runtime_module = pytest.importorskip("isola._runtime")

_FETCH_SCRIPT = (
    "import httpx2\n"
    "\n"
    "def main(url):\n"
    "\tresp = httpx2.get(url)\n"
    "\treturn [resp.status_code, resp.headers.get('x-test'), resp.text]\n"
)


def test_json_stream_from_iterable_roundtrip() -> None:
    stream_arg = isola.StreamArg.from_iterable([1, 2], capacity=1)
    assert stream_arg.name is None
    assert stream_arg.producer_task is None


def test_strip_first_path_component_flattens_bundle_root() -> None:
    strip_first_path_component = runtime_module._strip_first_path_component  # ruff:ignore[private-member-access]

    assert (
        strip_first_path_component("isola-python-runtime/bin/python.wasm")
        == "bin/python.wasm"
    )
    assert strip_first_path_component("isola-python-runtime") is None


@pytest.mark.asyncio
async def test_runtime_extraction_rejects_escaping_symlink(tmp_path: Path) -> None:
    archive = io.BytesIO()
    with tarfile.open(fileobj=archive, mode="w:gz") as tf:
        link = tarfile.TarInfo("isola-python-runtime/lib/escape")
        link.type = tarfile.SYMTYPE
        link.linkname = "../../../outside"
        tf.addfile(link)

    with pytest.raises(RuntimeError, match="symlink target escapes archive"):
        await runtime_module._extract_tarball(  # ruff:ignore[private-member-access]
            archive.getvalue(), tmp_path / "runtime"
        )


@pytest.mark.asyncio
async def test_runtime_extraction_rejects_member_below_symlink(tmp_path: Path) -> None:
    archive = io.BytesIO()
    with tarfile.open(fileobj=archive, mode="w:gz") as tf:
        link = tarfile.TarInfo("isola-python-runtime/lib/link")
        link.type = tarfile.SYMTYPE
        link.linkname = "target"
        tf.addfile(link)

        payload = b"payload"
        member = tarfile.TarInfo("isola-python-runtime/lib/link/payload")
        member.size = len(payload)
        tf.addfile(member, io.BytesIO(payload))

    with pytest.raises(RuntimeError, match="archive entry traverses symlink"):
        await runtime_module._extract_tarball(  # ruff:ignore[private-member-access]
            archive.getvalue(), tmp_path / "runtime"
        )


def _resolve_runtime_paths() -> tuple[Path, Path]:
    workspace_root = Path(__file__).resolve().parents[3]
    runtime_dir = workspace_root / "target"
    wasm_path = runtime_dir / "python.wasm"
    if not wasm_path.is_file():
        message = (
            f"missing runtime wasm at '{wasm_path}', build with `cargo xtask build-all`"
        )
        pytest.skip(message)

    wasi_runtime = os.environ.get("WASI_PYTHON_RUNTIME")
    if wasi_runtime is None:
        lib_dir = runtime_dir / "wasm32-wasip2" / "wasi-deps" / "usr" / "local" / "lib"
    else:
        lib_dir = Path(wasi_runtime) / "lib"

    if not lib_dir.is_dir():
        message = (
            f"missing runtime libs at '{lib_dir}', "
            "set WASI_PYTHON_RUNTIME or build runtime deps"
        )
        pytest.skip(message)

    return runtime_dir, lib_dir


def _resolve_js_runtime_dir() -> Path:
    workspace_root = Path(__file__).resolve().parents[3]
    runtime_dir = workspace_root / "target"
    wasm_path = runtime_dir / "js.wasm"
    if not wasm_path.is_file():
        message = (
            f"missing JS runtime wasm at '{wasm_path}', "
            "build with `cargo xtask build-js`"
        )
        pytest.skip(message)
    return runtime_dir


@pytest.mark.asyncio
async def test_context_creation_smoke() -> None:
    async with isola.SandboxContext():
        pass


@pytest.mark.asyncio
async def test_async_context_managers_smoke() -> None:
    runtime_dir, lib_dir = _resolve_runtime_paths()
    template = await isola.build_template(
        "python",
        runtime_path=runtime_dir,
        max_memory=64 * 1024 * 1024,
        runtime_lib_dir=lib_dir,
    )

    async with template.create() as sandbox:
        await sandbox.load_script("def ping():\n\treturn 'ok'")
        result = await sandbox.run("ping")
        assert result == "ok"


@pytest.mark.asyncio
async def test_async_context_managers_js_runtime_smoke() -> None:
    runtime_dir = _resolve_js_runtime_dir()
    template = await isola.build_template("js", runtime_path=runtime_dir)

    async with template.create() as sandbox:
        await sandbox.load_script("function ping() { return 'ok'; }")
        result = await sandbox.run("ping")
        assert result == "ok"


@pytest.mark.asyncio
async def test_initialize_template_rejects_unknown_runtime() -> None:
    with pytest.raises(isola.InvalidArgumentError, match="unsupported runtime"):
        await isola.build_template(cast("str", "ruby"), runtime_path=".")


@pytest.mark.asyncio
async def test_asyncio_timeout_wrapper() -> None:
    runtime_dir, lib_dir = _resolve_runtime_paths()
    template = await isola.build_template(
        "python",
        runtime_path=runtime_dir,
        max_memory=64 * 1024 * 1024,
        runtime_lib_dir=lib_dir,
    )

    async with template.create() as sandbox:
        await sandbox.load_script(
            "import time\n\ndef slow():\n\ttime.sleep(0.2)\n\treturn 1\n"
        )

        with pytest.raises(asyncio.TimeoutError):
            await asyncio.wait_for(sandbox.run("slow"), timeout=0.001)


@pytest.mark.asyncio
async def test_can_start_sandbox_and_execute_code() -> None:
    runtime_dir, lib_dir = _resolve_runtime_paths()
    template = await isola.build_template(
        "python",
        runtime_path=runtime_dir,
        max_memory=64 * 1024 * 1024,
        runtime_lib_dir=lib_dir,
    )

    async with template.create() as sandbox:
        await sandbox.load_script(
            "def add(a, b):\n"
            "\treturn a + b\n"
            "\n"
            "def stream_values(n):\n"
            "\tfor i in range(n):\n"
            "\t\tyield i"
        )

        add_result = await sandbox.run("add", 1, 2)
        assert add_result == 3

        stream_result = await sandbox.run("stream_values", 3)
        assert stream_result is None


@pytest.mark.asyncio
async def test_run_stream_yields_events() -> None:
    runtime_dir, lib_dir = _resolve_runtime_paths()
    template = await isola.build_template(
        "python",
        runtime_path=runtime_dir,
        max_memory=64 * 1024 * 1024,
        runtime_lib_dir=lib_dir,
    )

    async with template.create() as sandbox:
        await sandbox.load_script("def emit():\n\tprint('hello')\n\treturn 7\n")

        events = [event async for event in sandbox.run_stream("emit")]
        assert events
        assert any(isinstance(event, isola.StdoutEvent) for event in events)
        end_events = [event for event in events if isinstance(event, isola.EndEvent)]
        assert len(end_events) == 1
        assert end_events[0].data == 7


@pytest.mark.asyncio
async def test_template_create_hostcalls_json_roundtrip() -> None:
    class _FakeCore:
        def __init__(self) -> None:
            self.callback: Callable[[str, object], None] | None = None
            self.hostcall_handler: Callable[[str, object], Awaitable[object]] | None = (
                None
            )
            self.hostcall_loop: asyncio.AbstractEventLoop | None = None
            self.http_handler: (
                Callable[
                    [str, str, dict[str, str], bytes | None],
                    Awaitable[tuple[int, dict[str, str], str, object]],
                ]
                | None
            ) = None
            self.http_loop: asyncio.AbstractEventLoop | None = None

        def set_callback(self, callback: Callable[[str, object], None] | None) -> None:
            self.callback = callback

        def set_hostcall_handler(
            self,
            callback: Callable[[str, object], Awaitable[object]] | None,
            event_loop: asyncio.AbstractEventLoop | None,
        ) -> None:
            self.hostcall_handler = callback
            self.hostcall_loop = event_loop

        def set_http_handler(
            self,
            callback: Callable[
                [str, str, dict[str, str], bytes | None],
                Awaitable[tuple[int, dict[str, str], str, object]],
            ]
            | None,
            event_loop: asyncio.AbstractEventLoop | None,
        ) -> None:
            self.http_handler = callback
            self.http_loop = event_loop

        def close(self) -> None:
            pass

    class _FakeContextCore:
        def __init__(self, sandbox_core: _FakeCore) -> None:
            self.sandbox_core = sandbox_core

        async def instantiate(self) -> _FakeCore:
            return self.sandbox_core

    core = _FakeCore()
    template = isola.SandboxTemplate(cast("Any", _FakeContextCore(core)))

    async def lookup_user(payload: object) -> object:
        assert payload == {"user_id": 7}
        await asyncio.sleep(0)
        return {"id": 7, "name": "user-7"}

    await template.instantiate(hostcalls={"lookup_user": lookup_user})

    callback = core.hostcall_handler
    assert callback is not None
    assert core.hostcall_loop is asyncio.get_running_loop()
    assert await callback("lookup_user", {"user_id": 7}) == {"id": 7, "name": "user-7"}
    with pytest.raises(ValueError, match="unsupported hostcall: missing"):
        await callback("missing", {})

    empty_core = _FakeCore()
    empty_template = isola.SandboxTemplate(cast("Any", _FakeContextCore(empty_core)))
    await empty_template.instantiate()
    assert empty_core.hostcall_handler is None
    assert empty_core.hostcall_loop is None

    invalid_core = _FakeCore()
    invalid_template = isola.SandboxTemplate(
        cast("Any", _FakeContextCore(invalid_core))
    )
    with pytest.raises(TypeError, match="hostcall handler for 'lookup' must"):
        await invalid_template.instantiate(hostcalls=cast("Any", {"lookup": object()}))


@pytest.mark.asyncio
async def test_run_stream_flushes_trailing_scheduled_events() -> None:
    class _FakeCore:
        def __init__(self) -> None:
            self.callback: Callable[[str, object], None] | None = None
            self.http_handler: (
                Callable[
                    [str, str, dict[str, str], bytes | None],
                    Awaitable[tuple[int, dict[str, str], str, object]],
                ]
                | None
            ) = None
            self.http_loop: asyncio.AbstractEventLoop | None = None

        def set_callback(self, callback: Callable[[str, object], None] | None) -> None:
            self.callback = callback

        def set_http_handler(
            self,
            callback: Callable[
                [str, str, dict[str, str], bytes | None],
                Awaitable[tuple[int, dict[str, str], str, object]],
            ]
            | None,
            event_loop: asyncio.AbstractEventLoop | None,
        ) -> None:
            self.http_handler = callback
            self.http_loop = event_loop

        async def run(
            self, func: str, args: list[tuple[str, str | None, object]]
        ) -> None:
            _ = func
            _ = args
            callback = self.callback
            assert callback is not None
            callback("stdout", "hello")
            callback("end", 7)

        def close(self) -> None:
            pass

    core = _FakeCore()
    sandbox = isola.Sandbox(cast("Any", core))

    events = [event async for event in sandbox.run_stream("emit")]
    assert len(events) == 2
    assert isinstance(events[0], isola.StdoutEvent)
    assert events[0].data == "hello"
    assert isinstance(events[1], isola.EndEvent)
    assert events[1].data == 7


@pytest.mark.asyncio
async def test_run_treats_python_list_as_single_json_argument() -> None:
    class _FakeCore:
        def __init__(self) -> None:
            self.callback: Callable[[str, object], None] | None = None
            self.http_handler: (
                Callable[
                    [str, str, dict[str, str], bytes | None],
                    Awaitable[tuple[int, dict[str, str], str, object]],
                ]
                | None
            ) = None
            self.http_loop: asyncio.AbstractEventLoop | None = None
            self.args: list[tuple[str, str | None, object]] | None = None

        def set_callback(self, callback: Callable[[str, object], None] | None) -> None:
            self.callback = callback

        def set_http_handler(
            self,
            callback: Callable[
                [str, str, dict[str, str], bytes | None],
                Awaitable[tuple[int, dict[str, str], str, object]],
            ]
            | None,
            event_loop: asyncio.AbstractEventLoop | None,
        ) -> None:
            self.http_handler = callback
            self.http_loop = event_loop

        async def run(
            self, func: str, args: list[tuple[str, str | None, object]]
        ) -> SimpleNamespace:
            _ = func
            self.args = args
            return SimpleNamespace(final_json="[1, 2, 3]")

        def close(self) -> None:
            pass

    core = _FakeCore()
    sandbox = isola.Sandbox(cast("Any", core))

    result = await sandbox.run("echo", [1, 2, 3])

    assert result == [1, 2, 3]
    assert core.args == [("json", None, [1, 2, 3])]
    assert core.callback is None


@pytest.mark.asyncio
async def test_run_maps_kwargs_to_named_arguments() -> None:
    class _FakeCore:
        def __init__(self) -> None:
            self.callback: Callable[[str, object], None] | None = None
            self.http_handler: (
                Callable[
                    [str, str, dict[str, str], bytes | None],
                    Awaitable[tuple[int, dict[str, str], str, object]],
                ]
                | None
            ) = None
            self.http_loop: asyncio.AbstractEventLoop | None = None
            self.args: list[tuple[str, str | None, object]] | None = None

        def set_callback(self, callback: Callable[[str, object], None] | None) -> None:
            self.callback = callback

        def set_http_handler(
            self,
            callback: Callable[
                [str, str, dict[str, str], bytes | None],
                Awaitable[tuple[int, dict[str, str], str, object]],
            ]
            | None,
            event_loop: asyncio.AbstractEventLoop | None,
        ) -> None:
            self.http_handler = callback
            self.http_loop = event_loop

        async def run(
            self, func: str, args: list[tuple[str, str | None, object]]
        ) -> SimpleNamespace:
            _ = func
            self.args = args
            return SimpleNamespace(final_json=None)

        def close(self) -> None:
            pass

    core = _FakeCore()
    sandbox = isola.Sandbox(cast("Any", core))

    second = isola.Arg(2)
    await sandbox.run("echo", 1, second=second, third=3)

    assert core.args == [("json", None, 1), ("json", "second", 2), ("json", "third", 3)]
    assert second.name is None

    values = isola.StreamArg.from_iterable([1, 2])
    await sandbox.run("echo", values=values)
    assert core.args is not None
    assert core.args[0][:2] == ("stream", "values")
    assert values.name is None


@pytest.mark.asyncio
async def test_invalid_later_arg_does_not_start_stream_producer() -> None:
    class _FakeCore:
        def __init__(self) -> None:
            self.callback: Callable[[str, object], None] | None = None
            self.http_handler: (
                Callable[
                    [str, str, dict[str, str], bytes | None],
                    Awaitable[tuple[int, dict[str, str], str, object]],
                ]
                | None
            ) = None
            self.http_loop: asyncio.AbstractEventLoop | None = None

        def set_callback(self, callback: Callable[[str, object], None] | None) -> None:
            self.callback = callback

        def set_http_handler(
            self,
            callback: Callable[
                [str, str, dict[str, str], bytes | None],
                Awaitable[tuple[int, dict[str, str], str, object]],
            ]
            | None,
            event_loop: asyncio.AbstractEventLoop | None,
        ) -> None:
            self.http_handler = callback
            self.http_loop = event_loop

        async def run(
            self, func: str, args: list[tuple[str, str | None, object]]
        ) -> None:
            _ = self
            _ = func
            _ = args
            message = "sandbox.run() should not be called when argument encoding fails"
            raise AssertionError(message)

        def close(self) -> None:
            pass

    started = asyncio.Event()

    async def values() -> AsyncIterator[int]:
        started.set()
        await asyncio.sleep(0)
        yield 1

    stream_arg = isola.StreamArg.from_async_iterable(values())
    sandbox = isola.Sandbox(cast("Any", _FakeCore()))

    with pytest.raises(TypeError):
        await sandbox.run("emit", stream_arg, object())

    await asyncio.sleep(0)
    assert not started.is_set()
    assert stream_arg.producer_task is None


@pytest.mark.asyncio
async def test_run_propagates_stream_producer_failure() -> None:
    class _FakeCore:
        @staticmethod
        async def run(
            func: str, args: list[tuple[str, str | None, object]]
        ) -> SimpleNamespace:
            _ = func
            _ = args
            await asyncio.sleep(0)
            return SimpleNamespace(final_json=None)

        @staticmethod
        def close() -> None:
            pass

    async def values() -> AsyncIterator[int]:
        await asyncio.sleep(0)
        yield 1
        message = "producer failed"
        raise RuntimeError(message)

    sandbox = isola.Sandbox(cast("Any", _FakeCore()))
    stream_arg = isola.StreamArg.from_async_iterable(values())

    with pytest.raises(RuntimeError, match="producer failed"):
        await sandbox.run("consume", stream_arg)


@pytest.mark.asyncio
async def test_two_sandboxes_can_run_concurrently() -> None:
    runtime_dir, lib_dir = _resolve_runtime_paths()
    template = await isola.build_template(
        "python",
        runtime_path=runtime_dir,
        max_memory=64 * 1024 * 1024,
        runtime_lib_dir=lib_dir,
    )

    async with template.create() as sandbox_a, template.create() as sandbox_b:
        script = (
            "import time\n"
            "\n"
            "def identify(name, delay):\n"
            "\ttime.sleep(delay)\n"
            "\treturn name\n"
        )

        async def _run_one(sandbox: IsolaSandbox, name: str) -> str | None:
            await sandbox.load_script(script)
            result = await sandbox.run("identify", name, 0.05)
            return cast("str | None", result)

        result_a, result_b = await asyncio.gather(
            _run_one(sandbox_a, "sandbox-a"), _run_one(sandbox_b, "sandbox-b")
        )
        assert result_a == "sandbox-a"
        assert result_b == "sandbox-b"


@pytest.mark.asyncio
async def test_template_create_configures_http_handler() -> None:
    class _FakeCore:
        def __init__(self) -> None:
            self.callback: Callable[[str, object], None] | None = None
            self.http_handler: (
                Callable[
                    [str, str, dict[str, str], bytes | None],
                    Awaitable[tuple[int, dict[str, str], str, object]],
                ]
                | None
            ) = None
            self.http_loop: asyncio.AbstractEventLoop | None = None

        def configure(self, _: object) -> None:
            pass

        def set_callback(self, callback: Callable[[str, object], None] | None) -> None:
            self.callback = callback

        def set_http_handler(
            self,
            callback: Callable[
                [str, str, dict[str, str], bytes | None],
                Awaitable[tuple[int, dict[str, str], str, object]],
            ]
            | None,
            event_loop: asyncio.AbstractEventLoop | None,
        ) -> None:
            self.http_handler = callback
            self.http_loop = event_loop

        @staticmethod
        def set_hostcall_handler(
            callback: Callable[[str, object], Awaitable[object]] | None,
            event_loop: asyncio.AbstractEventLoop | None,
        ) -> None:
            _ = callback
            _ = event_loop

        def close(self) -> None:
            pass

    class _FakeContextCore:
        def __init__(self, sandbox_core: _FakeCore) -> None:
            self.sandbox_core = sandbox_core

        async def instantiate(self) -> _FakeCore:
            return self.sandbox_core

    core = _FakeCore()
    template = isola.SandboxTemplate(cast("Any", _FakeContextCore(core)))

    async def http(request: HttpRequest) -> HttpResponse:
        _ = request
        await asyncio.sleep(0)
        return cast("HttpResponse", isola.HttpResponse(status=204))

    await template.instantiate(http=http)

    assert core.http_handler is not None
    assert core.http_loop is asyncio.get_running_loop()


@pytest.mark.asyncio
async def test_template_create_rejects_invalid_http_handler() -> None:
    class _FakeCore:
        def configure(self, _: object) -> None:
            pass

        def set_callback(self, _: Callable[[str, object], None] | None) -> None:
            pass

        @staticmethod
        def set_http_handler(
            callback: Callable[
                [str, str, dict[str, str], bytes | None],
                Awaitable[tuple[int, dict[str, str], str, object]],
            ]
            | None,
            event_loop: asyncio.AbstractEventLoop | None,
        ) -> None:
            _ = callback
            _ = event_loop

        @staticmethod
        def set_hostcall_handler(
            callback: Callable[[str, object], Awaitable[object]] | None,
            event_loop: asyncio.AbstractEventLoop | None,
        ) -> None:
            _ = callback
            _ = event_loop

        def close(self) -> None:
            pass

    class _FakeContextCore:
        @staticmethod
        async def instantiate() -> _FakeCore:
            return _FakeCore()

    template = isola.SandboxTemplate(cast("Any", _FakeContextCore()))

    invalid_http: Any = False
    with pytest.raises(TypeError, match="http must be an async callable"):
        await template.instantiate(http=invalid_http)

    legacy_options: Any = {"http_handler": object()}
    with pytest.raises(TypeError, match=r"unexpected sandbox option.*http_handler"):
        await template.instantiate(**legacy_options)


@pytest.mark.asyncio
async def test_sandbox_http_bytes_response_shape() -> None:
    runtime_dir, lib_dir = _resolve_runtime_paths()
    template = await isola.build_template(
        "python",
        runtime_path=runtime_dir,
        max_memory=64 * 1024 * 1024,
        runtime_lib_dir=lib_dir,
    )

    async def http(_: HttpRequest) -> HttpResponse:
        await asyncio.sleep(0)
        return cast(
            "HttpResponse",
            isola.HttpResponse(
                status=201,
                headers={"content-type": "text/plain", "x-test": "bytes"},
                body=b"ok",
            ),
        )

    async with template.create(http=http) as sandbox:
        await sandbox.load_script(_FETCH_SCRIPT)

        result = await sandbox.run("main", "https://example.test/bytes")
        assert result == [201, "bytes", "ok"]


@pytest.mark.asyncio
async def test_sandbox_hostcalls_roundtrip() -> None:
    runtime_dir, lib_dir = _resolve_runtime_paths()
    template = await isola.build_template(
        "python",
        runtime_path=runtime_dir,
        max_memory=64 * 1024 * 1024,
        runtime_lib_dir=lib_dir,
    )

    async def lookup_user(payload: object) -> object:
        assert payload == {"user_id": 7}
        await asyncio.sleep(0)
        return {"user_id": 7, "name": "user-7"}

    async with template.create(hostcalls={"lookup_user": lookup_user}) as sandbox:
        await sandbox.load_script(
            "from sandbox.asyncio import hostcall\n"
            "\n"
            "async def main(user_id):\n"
            "\treturn await hostcall('lookup_user', {'user_id': user_id})\n"
        )

        result = await sandbox.run("main", 7)
        assert result == {"user_id": 7, "name": "user-7"}


@pytest.mark.asyncio
async def test_sandbox_http_stream_response_shape() -> None:
    runtime_dir, lib_dir = _resolve_runtime_paths()
    template = await isola.build_template(
        "python",
        runtime_path=runtime_dir,
        max_memory=64 * 1024 * 1024,
        runtime_lib_dir=lib_dir,
    )

    async def response_chunks() -> AsyncIterator[bytes]:
        await asyncio.sleep(0)
        yield b"a"
        await asyncio.sleep(0)
        yield b"b"

    async def http(req: HttpRequest) -> HttpResponse:
        assert req.method == "GET"
        assert req.url == "https://example.test/stream"
        await asyncio.sleep(0)
        return cast(
            "HttpResponse",
            isola.HttpResponse(
                status=200,
                headers={"content-type": "text/plain", "x-test": "stream"},
                body=response_chunks(),
            ),
        )

    async with template.create(http=http) as sandbox:
        await sandbox.load_script(_FETCH_SCRIPT)

        result = await sandbox.run("main", "https://example.test/stream")
        assert result == [200, "stream", "ab"]


@pytest.mark.asyncio
async def test_sandbox_caller_timeout_does_not_break_following_requests() -> None:
    runtime_dir, lib_dir = _resolve_runtime_paths()
    template = await isola.build_template(
        "python",
        runtime_path=runtime_dir,
        max_memory=64 * 1024 * 1024,
        runtime_lib_dir=lib_dir,
    )

    async def warmup_handler(_: HttpRequest) -> HttpResponse:
        await asyncio.sleep(0)
        return cast("HttpResponse", isola.HttpResponse(status=200, body=b"ok"))

    async with template.create(http=warmup_handler) as warmup:
        await warmup.load_script(_FETCH_SCRIPT)
        result = await warmup.run("main", "https://example.test/warmup")
        assert result == [200, None, "ok"]

    for _ in range(4):

        async def hanging_handler(_: HttpRequest) -> HttpResponse:
            await asyncio.Event().wait()
            message = "unreachable"
            raise AssertionError(message)

        async with template.create(http=hanging_handler) as sandbox:
            await sandbox.load_script(_FETCH_SCRIPT)
            with pytest.raises(asyncio.TimeoutError):
                await asyncio.wait_for(
                    sandbox.run("main", "https://example.test/hang"), timeout=0.05
                )

        await asyncio.sleep(0.05)

    async def recovery_handler(_: HttpRequest) -> HttpResponse:
        await asyncio.sleep(0)
        return cast("HttpResponse", isola.HttpResponse(status=200, body=b"ok"))

    async with template.create(http=recovery_handler) as sandbox:
        await sandbox.load_script(_FETCH_SCRIPT)
        result = await sandbox.run("main", "https://example.test/recovery")
        assert result == [200, None, "ok"]
