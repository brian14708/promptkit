# pyright: reportPrivateUsage=false

from __future__ import annotations

import asyncio
from collections.abc import AsyncIterable, Awaitable, Callable
from dataclasses import dataclass, field
from itertools import starmap
from json import loads
from os import PathLike, fspath
from typing import TYPE_CHECKING, Literal, TypeAlias, cast
from typing_extensions import Self, TypedDict, Unpack

import httpx2

from isola._isola import _ContextCore, _StreamCore

if TYPE_CHECKING:
    from collections.abc import AsyncIterator, Iterable, Sequence

    from isola._isola import _RunResultCore, _SandboxCore

JsonScalar = bool | int | float | str | None
JsonValue = JsonScalar | list["JsonValue"] | dict[str, "JsonValue"]
RuntimeName = Literal["python", "js"]
BytesLike = bytes | bytearray | memoryview
Pathish = str | PathLike[str]
HttpBody = BytesLike | AsyncIterable[BytesLike] | None
HostcallHandler = Callable[[JsonValue], Awaitable[object]]
Hostcalls = dict[str, HostcallHandler]


@dataclass(frozen=True, slots=True)
class ResultEvent:
    data: JsonValue


@dataclass(frozen=True, slots=True)
class EndEvent:
    data: JsonValue | None


@dataclass(frozen=True, slots=True)
class StdoutEvent:
    data: str


@dataclass(frozen=True, slots=True)
class StderrEvent:
    data: str


@dataclass(frozen=True, slots=True)
class ErrorEvent:
    data: str


@dataclass(frozen=True, slots=True)
class LogEvent:
    data: str


Event = ResultEvent | EndEvent | StdoutEvent | StderrEvent | ErrorEvent | LogEvent


@dataclass(frozen=True, slots=True)
class HttpRequest:
    method: str
    url: str
    headers: dict[str, str]
    body: bytes | None


@dataclass(slots=True)
class HttpResponse:
    status: int
    headers: dict[str, str] = field(default_factory=dict)
    body: HttpBody = None


HttpHandler: TypeAlias = Callable[[HttpRequest], Awaitable[object]]
HttpHandlerConfig: TypeAlias = HttpHandler | Literal[True] | None
_SANDBOX_CONFIG_KEYS = frozenset({"max_memory", "mounts", "env", "http", "hostcalls"})


async def _default_httpx2_handler(request: HttpRequest) -> HttpResponse:
    client = httpx2.AsyncClient()
    try:
        outbound_request = client.build_request(
            request.method, request.url, headers=request.headers, content=request.body
        )
        response = await client.send(outbound_request, stream=True)
    except Exception:
        await client.aclose()
        raise

    async def _stream_body() -> AsyncIterable[bytes]:
        try:
            async for chunk in response.aiter_bytes():
                yield chunk
        finally:
            await response.aclose()
            await client.aclose()

    return HttpResponse(
        status=response.status_code, headers=dict(response.headers), body=_stream_body()
    )


@dataclass(frozen=True, slots=True)
class MountConfig:
    host: Pathish
    guest: str
    dir_perms: Literal["read", "write", "read-write"] = "read"
    file_perms: Literal["read", "write", "read-write"] = "read"

    def to_dict(self) -> dict[str, str]:
        return {
            "host": _normalize_path(self.host, key="host"),
            "guest": self.guest,
            "dir_perms": self.dir_perms,
            "file_perms": self.file_perms,
        }


class TemplateConfig(TypedDict, total=False):
    runtime_path: Pathish | None
    cache_dir: Pathish | None
    max_memory: int | None
    prelude: str | None
    runtime_lib_dir: Pathish | None
    mounts: list[MountConfig] | None
    env: dict[str, str]


class SandboxConfig(TypedDict, total=False):
    max_memory: int | None
    mounts: list[MountConfig] | None
    env: dict[str, str]
    http: HttpHandlerConfig
    hostcalls: Hostcalls | None


@dataclass(frozen=True, slots=True)
class Arg:
    value: object
    name: str | None = None


@dataclass(slots=True)
class _StreamArgState:
    core: _StreamCore
    source: AsyncIterable[object] | None
    producer_task: asyncio.Task[None] | None


class StreamArg:
    def __init__(
        self,
        core: _StreamCore,
        *,
        name: str | None = None,
        source: AsyncIterable[object] | None = None,
        producer_task: asyncio.Task[None] | None = None,
        _state: _StreamArgState | None = None,
    ) -> None:
        self._state = _state or _StreamArgState(core, source, producer_task)
        self._name = name

    @property
    def name(self) -> str | None:
        return self._name

    @property
    def stream_core(self) -> _StreamCore:
        return self._state.core

    @property
    def producer_task(self) -> asyncio.Task[None] | None:
        return self._state.producer_task

    def start_producer(self) -> asyncio.Task[None] | None:
        if self._state.producer_task is not None or self._state.source is None:
            return self._state.producer_task

        source = self._state.source
        self._state.source = None

        async def _produce() -> None:
            try:
                async for item in source:
                    await self._state.core.push_async(item)
            finally:
                self._state.core.end()

        self._state.producer_task = asyncio.create_task(_produce())
        return self._state.producer_task

    def _with_name(self, name: str) -> StreamArg:
        return StreamArg(self._state.core, name=name, _state=self._state)

    @classmethod
    def from_async_iterable(
        cls,
        values: AsyncIterable[object],
        *,
        name: str | None = None,
        capacity: int = 1024,
    ) -> StreamArg:
        core = _StreamCore(capacity)
        return cls(core, name=name, source=values)

    @classmethod
    def from_iterable(
        cls, values: Iterable[object], *, name: str | None = None, capacity: int = 1024
    ) -> StreamArg:
        try:
            asyncio.get_running_loop()
        except RuntimeError:
            buffered = list(values)
            core = _StreamCore(max(capacity, len(buffered), 1))
            for item in buffered:
                core.push(item)
            core.end()
            return cls(core, name=name)

        async def _iterate() -> AsyncIterable[object]:
            await asyncio.sleep(0)
            for item in values:
                yield item

        return cls.from_async_iterable(_iterate(), name=name, capacity=capacity)


RunArg = Arg | StreamArg | JsonValue


def _normalize_mounts(
    mounts: object, *, key: str = "mounts"
) -> list[dict[str, str]] | None:
    if mounts is None:
        return None
    if not isinstance(mounts, list):
        msg = f"{key} must be a list[MountConfig] or None"
        raise TypeError(msg)

    mount_items = cast("list[object]", mounts)
    encoded: list[dict[str, str]] = []
    for mount_obj in mount_items:
        if not isinstance(mount_obj, MountConfig):
            msg = f"{key} entries must be MountConfig"
            raise TypeError(msg)
        encoded.append(mount_obj.to_dict())
    return encoded


def _normalize_path(value: object, *, key: str) -> str:
    if isinstance(value, str):
        return value
    if isinstance(value, bytes):
        msg = f"{key} must be str | os.PathLike[str], not bytes"
        raise TypeError(msg)
    if isinstance(value, PathLike):
        path_like_value = cast("PathLike[str] | PathLike[bytes]", value)
        raw_path = fspath(path_like_value)
        if isinstance(raw_path, bytes):
            msg = f"{key} must be str | os.PathLike[str], not bytes"
            raise TypeError(msg)
        return raw_path
    msg = f"{key} must be str | os.PathLike[str]"
    raise TypeError(msg)


def _normalize_optional_path(value: object, *, key: str) -> str | None:
    if value is None:
        return None
    return _normalize_path(value, key=key)


def _configure_core(
    core: _ContextCore | _SandboxCore, patch: dict[str, object]
) -> None:
    if patch:
        core.configure(patch)


def _resolve_http_handler(handler: object) -> HttpHandler | None:
    if handler is None:
        return None
    if handler is True:
        return _default_httpx2_handler
    if isinstance(handler, bool):
        msg = "http must be an async callable, True, or None"
        raise TypeError(msg)
    if not callable(handler):
        msg = "http must be an async callable, True, or None"
        raise TypeError(msg)
    return cast("HttpHandler", handler)


class SandboxContext:
    def __init__(self) -> None:
        self._core = _ContextCore()

    async def compile_template(
        self,
        runtime: RuntimeName,
        *,
        version: str | None = None,
        **kwargs: Unpack[TemplateConfig],
    ) -> SandboxTemplate:
        runtime_path = kwargs.pop("runtime_path", None)

        if runtime_path is None:
            from isola._runtime import (  # ruff:ignore[import-outside-top-level]
                resolve_runtime,
            )

            defaults = await resolve_runtime(runtime, version=version)
            resolved: dict[str, object] = {**defaults, **kwargs}
        else:
            resolved = dict(kwargs)
            resolved["runtime_path"] = runtime_path

        actual_runtime_path = resolved.pop("runtime_path", None)
        if actual_runtime_path is None:
            msg = "runtime_path must be provided or resolvable via auto-download"
            raise ValueError(msg)

        patch: dict[str, object] = dict(resolved)
        if "cache_dir" not in patch or patch["cache_dir"] is None:
            from isola._runtime import (  # ruff:ignore[import-outside-top-level]
                _cache_base,
            )

            patch["cache_dir"] = str(_cache_base() / "isola" / "cache")
        if "cache_dir" in patch:
            patch["cache_dir"] = _normalize_optional_path(
                patch["cache_dir"], key="cache_dir"
            )
        if "runtime_lib_dir" in patch:
            patch["runtime_lib_dir"] = _normalize_optional_path(
                patch["runtime_lib_dir"], key="runtime_lib_dir"
            )
        if "mounts" in patch:
            patch["mounts"] = _normalize_mounts(patch["mounts"])
        _configure_core(self._core, patch)
        normalized_runtime_path = _normalize_path(
            actual_runtime_path, key="runtime_path"
        )
        await self._core.initialize_template(normalized_runtime_path, runtime)
        return SandboxTemplate(self._core)

    async def __aenter__(self) -> Self:
        return self

    async def __aexit__(self, *_: object) -> None:
        self.close()

    def close(self) -> None:
        self._core.close()


async def build_template(
    runtime: RuntimeName,
    *,
    version: str | None = None,
    **kwargs: Unpack[TemplateConfig],
) -> SandboxTemplate:
    context = SandboxContext()
    return await context.compile_template(runtime, version=version, **kwargs)


class SandboxTemplate:
    def __init__(self, core: _ContextCore) -> None:
        self._core = core

    def create(self, **kwargs: Unpack[SandboxConfig]) -> _SandboxContext:
        return _SandboxContext(self, kwargs)

    async def instantiate(self, **kwargs: Unpack[SandboxConfig]) -> Sandbox:
        unexpected = sorted(set(kwargs).difference(_SANDBOX_CONFIG_KEYS))
        if unexpected:
            names = ", ".join(repr(name) for name in unexpected)
            msg = f"unexpected sandbox option(s): {names}"
            raise TypeError(msg)

        core = await self._core.instantiate()
        sandbox = Sandbox(core)

        patch: dict[str, object] = {}
        if "max_memory" in kwargs:
            patch["max_memory"] = kwargs["max_memory"]
        if "mounts" in kwargs:
            patch["mounts"] = _normalize_mounts(kwargs["mounts"])
        if "env" in kwargs:
            patch["env"] = kwargs["env"]
        _configure_core(sandbox._core, patch)  # ruff:ignore[private-member-access]

        hostcalls = kwargs.get("hostcalls")
        http_handler = _resolve_http_handler(kwargs.get("http"))
        sandbox._set_hostcalls(hostcalls)  # ruff:ignore[private-member-access]
        sandbox._set_http_handler(http_handler)  # ruff:ignore[private-member-access]
        return sandbox


class _SandboxContext:
    def __init__(self, template: SandboxTemplate, kwargs: SandboxConfig) -> None:
        self._template = template
        self._kwargs = kwargs
        self._used = False
        self._sandbox: Sandbox | None = None

    async def __aenter__(self) -> Sandbox:
        if self._used:
            msg = "sandbox context cannot be entered more than once"
            raise RuntimeError(msg)
        self._used = True

        sandbox = await self._template.instantiate(**self._kwargs)
        await sandbox.__aenter__()
        self._sandbox = sandbox
        return sandbox

    async def __aexit__(self, *args: object) -> None:
        sandbox = self._sandbox
        if sandbox is None:
            return
        self._sandbox = None
        await sandbox.__aexit__(*args)


class Sandbox:
    def __init__(self, core: _SandboxCore) -> None:
        self._core = core
        self._stream_dispatches: dict[int, Callable[[str, object], None]] = {}
        self._next_stream_dispatch_id = 0
        self._hostcall_handler_dispatch: (
            Callable[[str, object], Awaitable[object]] | None
        ) = None
        self._http_handler_dispatch: (
            Callable[
                [str, str, dict[str, str], bytes | None],
                Awaitable[tuple[int, dict[str, str], str, object]],
            ]
            | None
        ) = None

    def _refresh_core_callback(self) -> None:
        if not self._stream_dispatches:
            self._core.set_callback(None)
            return
        if len(self._stream_dispatches) == 1:
            self._core.set_callback(next(iter(self._stream_dispatches.values())))
            return

        def _dispatch(kind: str, data: object) -> None:
            # Stream listeners can change while events are emitted.
            stream_dispatches = tuple(self._stream_dispatches.values())
            for stream_dispatch in stream_dispatches:
                stream_dispatch(kind, data)

        self._core.set_callback(_dispatch)

    def _set_hostcalls(self, hostcalls: Hostcalls | None) -> None:
        if hostcalls is None:
            self._hostcall_handler_dispatch = None
            self._core.set_hostcall_handler(None, None)
            return

        loop = asyncio.get_running_loop()
        raw_hostcalls = cast("dict[object, object]", dict(hostcalls))
        dispatch_hostcalls: Hostcalls = {}
        for call_type, handler in raw_hostcalls.items():
            if not isinstance(call_type, str) or not call_type:
                msg = "hostcall names must be non-empty strings"
                raise TypeError(msg)
            if not callable(handler):
                msg = f"hostcall handler for {call_type!r} must be an async callable"
                raise TypeError(msg)
            dispatch_hostcalls[call_type] = cast("HostcallHandler", handler)

        async def _dispatch(call_type: str, payload: object) -> object:
            handler = dispatch_hostcalls.get(call_type)
            if handler is None:
                msg = f"unsupported hostcall: {call_type}"
                raise ValueError(msg)
            return await handler(cast("JsonValue", payload))

        self._hostcall_handler_dispatch = _dispatch
        self._core.set_hostcall_handler(_dispatch, loop)

    def _set_http_handler(
        self, handler: Callable[[HttpRequest], Awaitable[object]] | None
    ) -> None:
        if handler is None:
            self._http_handler_dispatch = None
            self._core.set_http_handler(None, None)
            return

        loop = asyncio.get_running_loop()

        async def _dispatch(
            method: str, url: str, headers: dict[str, str], body: bytes | None
        ) -> tuple[int, dict[str, str], str, object]:
            request = HttpRequest(method=method, url=url, headers=headers, body=body)
            response: object = await handler(request)
            if not isinstance(response, HttpResponse):
                msg = "http handler must return HttpResponse"
                raise TypeError(msg)
            status = cast("object", response.status)
            if (
                isinstance(status, bool)
                or not isinstance(status, int)
                or not 100 <= status <= 999
            ):
                msg = "http response status must be an integer from 100 to 999"
                raise ValueError(msg)
            raw_headers = cast("dict[object, object]", response.headers)
            if not all(
                isinstance(name, str) and isinstance(value, str)
                for name, value in raw_headers.items()
            ):
                msg = "http response headers must map strings to strings"
                raise TypeError(msg)
            headers = cast("dict[str, str]", raw_headers)
            body_mode, body_payload = _normalize_http_response_body(response.body)
            return (status, dict(headers), body_mode, body_payload)

        self._http_handler_dispatch = _dispatch
        self._core.set_http_handler(_dispatch, loop)

    async def load_script(self, code: str) -> None:
        await self._core.load_script(code)

    async def run(
        self, name: str, /, *args: RunArg, **kwargs: RunArg
    ) -> JsonValue | None:
        final_args = _merge_run_args(args, kwargs)
        result = await self._run_operation(name, final_args)
        if result.final_json is None:
            return None
        return cast("JsonValue", loads(result.final_json))

    async def _run_operation(
        self, name: str, args: Sequence[RunArg] | None = None
    ) -> _RunResultCore:
        encoded_args, producers = _encode_args(args)
        operation = self._core.run(name, encoded_args)

        try:
            result = await operation
        except BaseException:
            for producer in producers:
                producer.cancel()
            if producers:
                await asyncio.gather(*producers, return_exceptions=True)
            raise

        if producers:
            await asyncio.gather(*producers)
        return result

    async def run_stream(
        self, name: str, /, *args: RunArg, **kwargs: RunArg
    ) -> AsyncIterator[Event]:
        final_args = _merge_run_args(args, kwargs)
        completion = object()
        queue: asyncio.Queue[object] = asyncio.Queue()
        loop = asyncio.get_running_loop()
        pending_dispatches = 0
        operation_finished = False
        completion_enqueued = False

        def _finish_if_drained() -> None:
            nonlocal completion_enqueued
            if (
                operation_finished
                and pending_dispatches == 0
                and not completion_enqueued
            ):
                completion_enqueued = True
                queue.put_nowait(completion)

        def _enqueue(event: Event) -> None:
            nonlocal pending_dispatches
            queue.put_nowait(event)
            pending_dispatches -= 1
            _finish_if_drained()

        def _dispatch(kind: str, data: object) -> None:
            nonlocal pending_dispatches
            event: Event
            if kind == "result":
                if data is None:
                    return
                event = ResultEvent(data=cast("JsonValue", data))
            elif kind == "end":
                event = EndEvent(data=cast("JsonValue | None", data))
            elif kind == "stdout":
                if data is None:
                    return
                event = StdoutEvent(data=cast("str", data))
            elif kind == "stderr":
                if data is None:
                    return
                event = StderrEvent(data=cast("str", data))
            elif kind == "error":
                if data is None:
                    return
                event = ErrorEvent(data=cast("str", data))
            elif kind == "log":
                if data is None:
                    return
                event = LogEvent(data=cast("str", data))
            else:
                return
            pending_dispatches += 1
            loop.call_soon_threadsafe(_enqueue, event)

        stream_dispatch_id = self._next_stream_dispatch_id
        self._next_stream_dispatch_id += 1
        self._stream_dispatches[stream_dispatch_id] = _dispatch
        self._refresh_core_callback()

        async def _run_and_finish() -> _RunResultCore:
            nonlocal operation_finished
            try:
                return await self._run_operation(name, final_args)
            finally:
                operation_finished = True
                _finish_if_drained()

        run_task = asyncio.create_task(_run_and_finish())

        try:
            while True:
                event = await queue.get()
                if event is completion:
                    await run_task
                    break
                yield cast("Event", event)
        finally:
            self._stream_dispatches.pop(stream_dispatch_id, None)
            self._refresh_core_callback()
            if not run_task.done():
                run_task.cancel()
                await asyncio.gather(run_task, return_exceptions=True)

    async def __aenter__(self) -> Self:
        await self._core.start()
        return self

    async def __aexit__(self, *_: object) -> None:
        await self.aclose()

    def close(self) -> None:
        self._stream_dispatches.clear()
        self._hostcall_handler_dispatch = None
        self._http_handler_dispatch = None
        self._core.close()

    async def aclose(self) -> None:
        self._stream_dispatches.clear()
        self._hostcall_handler_dispatch = None
        self._http_handler_dispatch = None
        self._core.close()


def _encode_args(
    args: Sequence[object] | None,
) -> tuple[list[tuple[str, str | None, object]], list[asyncio.Task[None]]]:
    if args is None:
        return [], []

    encoded: list[tuple[str, str | None, object]] = []
    producers: list[asyncio.Task[None]] = []
    stream_args: list[StreamArg] = []

    for arg in args:
        if isinstance(arg, Arg):
            encoded.append(("json", arg.name, arg.value))
            continue

        if isinstance(arg, StreamArg):
            encoded.append(("stream", arg.name, arg.stream_core))
            stream_args.append(arg)
            continue

        if not isinstance(arg, (bool, int, float, str, list, dict)) and arg is not None:
            msg = f"invalid run argument type: {type(arg)!r}"
            raise TypeError(msg)
        encoded.append(("json", None, cast("object", arg)))

    for stream_arg in stream_args:
        producer = stream_arg.start_producer()
        if producer is not None:
            producers.append(producer)

    return encoded, producers


def _merge_run_args(
    args: tuple[RunArg, ...], kwargs: dict[str, RunArg]
) -> tuple[RunArg, ...]:
    if not kwargs:
        return args

    named_args = tuple(starmap(_normalize_keyword_arg, kwargs.items()))
    return args + named_args


def _normalize_keyword_arg(name: str, value: RunArg) -> RunArg:
    if isinstance(value, Arg):
        if value.name is not None and value.name != name:
            msg = (
                f"keyword argument {name!r} conflicts with explicit argument name "
                f"{value.name!r}"
            )
            raise TypeError(msg)
        return Arg(value.value, name=name)

    if isinstance(value, StreamArg):
        if value.name is not None and value.name != name:
            msg = (
                f"keyword argument {name!r} conflicts with explicit argument name "
                f"{value.name!r}"
            )
            raise TypeError(msg)
        return value._with_name(name)  # ruff:ignore[private-member-access]

    return Arg(value, name=name)


def _normalize_http_response_body(body: object) -> tuple[str, object]:
    if body is None:
        return ("none", None)

    if isinstance(body, bytes):
        return ("bytes", body)
    if isinstance(body, bytearray):
        return ("bytes", bytes(body))
    if isinstance(body, memoryview):
        return ("bytes", body.tobytes())

    if not isinstance(body, AsyncIterable):
        msg = "http response body must be bytes, AsyncIterable[bytes], or None"
        raise TypeError(msg)

    async def _stream_body(source: AsyncIterable[object]) -> AsyncIterable[bytes]:
        async for chunk in source:
            if isinstance(chunk, bytes):
                yield chunk
                continue
            if isinstance(chunk, bytearray):
                yield bytes(chunk)
                continue
            if isinstance(chunk, memoryview):
                yield chunk.tobytes()
                continue
            msg = "http response stream chunks must be bytes-like"
            raise TypeError(msg)

    source = cast("AsyncIterable[object]", body)
    return ("stream", _stream_body(source))
