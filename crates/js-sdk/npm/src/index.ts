import { decode, encode } from "cbor-x";
import { resolveRuntime } from "./_runtime.js";
// @ts-expect-error - napi bindings are generated at build time
import { ContextCore, type SandboxCore, StreamHandle } from "./isola.js";
import type {
  Event,
  Hostcalls,
  HttpHandler,
  HttpHandlerConfig,
  HttpRequest,
  HttpResponse,
  JsonValue,
  MountConfig,
  RunArg,
  RunKwargs,
  RuntimeName,
  SandboxOptions,
  StreamSource,
  TemplateOptions,
} from "./types.js";
import { Arg, StreamArg } from "./types.js";

export type {
  Event,
  HostcallHandler,
  Hostcalls,
  HttpHandler,
  HttpHandlerConfig,
  HttpRequest,
  HttpResponse,
  JsonValue,
  MountConfig,
  RunArg,
  RunKwargs,
  RuntimeName,
  SandboxOptions,
  StreamSource,
  TemplateOptions,
} from "./types.js";

export { Arg, StreamArg } from "./types.js";

// ---------------------------------------------------------------------------
// Wire format helpers
// ---------------------------------------------------------------------------

type WireArgument = [string, string | null, unknown];
type WireMountConfig = {
  host: string;
  guest: string;
  dir_perms?: "read" | "write" | "read-write";
  file_perms?: "read" | "write" | "read-write";
};
type NativeCallbackArgs = [string, string | null];
type NativeHostcallArgs = [string, Buffer];
type NativeHttpArgs = [string, string, Buffer, Buffer | null];
type NativeHttpResponse = {
  status: number;
  headers?: Record<string, string>;
  body?: Buffer | null;
};
type NativeRunResult = {
  resultJson: string[];
  finalJson?: string;
  stdout: string[];
  stderr: string[];
  logs: string[];
  errors: string[];
};

type NativeStreamHandle = InstanceType<typeof StreamHandle>;

interface EncodedRunArguments {
  wire: WireArgument[];
  streams: NativeStreamHandle[];
  producers: Promise<void>[];
}

const TEMPLATE_OPTION_KEYS = new Set([
  "runtimePath",
  "version",
  "cacheDir",
  "maxMemory",
  "prelude",
  "runtimeLibDir",
  "mounts",
  "env",
]);
const SANDBOX_OPTION_KEYS = new Set([
  "maxMemory",
  "mounts",
  "env",
  "hostcalls",
  "http",
]);

function validateOptionKeys(
  options: object | undefined,
  allowed: ReadonlySet<string>,
  kind: string,
): void {
  if (options === undefined) return;
  if (options === null || Array.isArray(options)) {
    throw new TypeError(`${kind} options must be an object`);
  }
  const unexpected = Object.keys(options)
    .filter((key) => !allowed.has(key))
    .sort();
  if (unexpected.length > 0) {
    throw new TypeError(
      `unexpected ${kind} option(s): ${unexpected.map((key) => `'${key}'`).join(", ")}`,
    );
  }
}

function encodeMounts(
  mounts: MountConfig[] | undefined,
): WireMountConfig[] | undefined {
  return mounts?.map((mount) => ({
    host: mount.host,
    guest: mount.guest,
    dir_perms: mount.dirPerms,
    file_perms: mount.filePerms,
  }));
}

async function pumpStream(
  source: StreamSource,
  stream: NativeStreamHandle,
): Promise<void> {
  try {
    for await (const value of source) {
      await stream.pushAsync(value);
    }
  } finally {
    stream.end();
  }
}

function encodeArgs(args?: readonly RunArg[]): EncodedRunArguments {
  const wire: WireArgument[] = [];
  const streams: NativeStreamHandle[] = [];
  const producers: Promise<void>[] = [];

  try {
    for (const arg of args ?? []) {
      if (arg instanceof Arg) {
        wire.push(["json", arg.name ?? null, arg.value]);
        continue;
      }
      if (arg instanceof StreamArg) {
        const stream = new StreamHandle(arg.capacity);
        const index = streams.push(stream) - 1;
        wire.push(["stream", arg.name ?? null, index]);
        producers.push(pumpStream(arg.values, stream));
        continue;
      }
      wire.push(["json", null, arg]);
    }
  } catch (error) {
    for (const stream of streams) stream.end();
    void Promise.allSettled(producers);
    throw error;
  }

  return { wire, streams, producers };
}

function normalizeKeywordArg(name: string, value: RunArg): RunArg {
  if (value instanceof Arg) {
    if (value.name !== undefined && value.name !== name) {
      throw new TypeError(
        `keyword argument '${name}' conflicts with explicit argument name '${value.name}'`,
      );
    }
    return new Arg(value.value, name);
  }
  if (value instanceof StreamArg) {
    if (value.name !== undefined && value.name !== name) {
      throw new TypeError(
        `keyword argument '${name}' conflicts with explicit argument name '${value.name}'`,
      );
    }
    return new StreamArg(value.values, name, value.capacity);
  }
  return new Arg(value, name);
}

function mergeRunArgs(args?: RunArg[], kwargs?: RunKwargs): readonly RunArg[] {
  if (args !== undefined && !Array.isArray(args)) {
    throw new TypeError(
      "sandbox args must be an array; pass kwargs as the third argument",
    );
  }

  if (!kwargs) return args ?? [];

  const merged = args ? [...args] : [];
  for (const [name, value] of Object.entries(kwargs)) {
    merged.push(normalizeKeywordArg(name, value));
  }
  return merged;
}

async function executeRun(
  core: InstanceType<typeof SandboxCore>,
  name: string,
  encoded: EncodedRunArguments,
): Promise<NativeRunResult> {
  const operation: Promise<NativeRunResult> =
    encoded.streams.length > 0
      ? core.runWithStreams(name, encoded.wire, encoded.streams)
      : core.run(name, encoded.wire);

  if (encoded.producers.length === 0) return operation;

  try {
    const results = await Promise.all([operation, ...encoded.producers]);
    return results[0] as NativeRunResult;
  } catch (error) {
    for (const stream of encoded.streams) stream.end();
    await Promise.allSettled([operation, ...encoded.producers]);
    throw error;
  }
}

function validateHostcalls(hostcalls: Hostcalls): void {
  for (const [name, handler] of Object.entries(hostcalls)) {
    if (name.length === 0) {
      throw new TypeError("hostcall names must not be empty");
    }
    if (typeof handler !== "function") {
      throw new TypeError(`hostcall handler for '${name}' must be a function`);
    }
  }
}

function unpackTuple<T extends unknown[]>(raw: unknown[]): T {
  if (raw.length === 1 && Array.isArray(raw[0])) {
    return raw[0] as T;
  }
  return raw as T;
}

function parseEvent(kind: string, data: string | null): Event | null {
  switch (kind) {
    case "result":
      return data != null
        ? { type: "result", data: JSON.parse(data) as JsonValue }
        : null;
    case "end":
      return {
        type: "end",
        data: data != null ? (JSON.parse(data) as JsonValue) : null,
      };
    case "stdout":
      return data != null ? { type: "stdout", data } : null;
    case "stderr":
      return data != null ? { type: "stderr", data } : null;
    case "error":
      return data != null ? { type: "error", data } : null;
    case "log":
      return data != null ? { type: "log", data } : null;
    default:
      return null;
  }
}

function eventKey(event: Event): string {
  return `${event.type}:${JSON.stringify(event.data)}`;
}

function resultToEvents(result: NativeRunResult): Event[] {
  const events: Event[] = [];

  for (const item of result.resultJson) {
    events.push({
      type: "result",
      data: JSON.parse(item) as JsonValue,
    });
  }

  for (const message of result.stdout) {
    events.push({ type: "stdout", data: message });
  }

  for (const message of result.stderr) {
    events.push({ type: "stderr", data: message });
  }

  for (const message of result.logs) {
    events.push({ type: "log", data: message });
  }

  for (const message of result.errors) {
    events.push({ type: "error", data: message });
  }

  events.push({
    type: "end",
    data:
      result.finalJson !== undefined
        ? (JSON.parse(result.finalJson) as JsonValue)
        : null,
  });

  return events;
}

async function defaultHttpHandler(request: HttpRequest): Promise<HttpResponse> {
  if (typeof fetch !== "function") {
    throw new Error(
      "global fetch is not available; pass a custom http handler",
    );
  }

  const response = await fetch(request.url, {
    method: request.method,
    headers: request.headers,
    body: request.body ?? undefined,
  });

  const body = Buffer.from(await response.arrayBuffer());
  return {
    status: response.status,
    headers: Object.fromEntries(response.headers.entries()),
    body,
  };
}

function normalizeHttpResponse(response: HttpResponse): NativeHttpResponse {
  if (response === null || typeof response !== "object") {
    throw new TypeError("http handler must return an HttpResponse object");
  }
  if (
    !Number.isInteger(response.status) ||
    response.status < 100 ||
    response.status > 999
  ) {
    throw new RangeError(
      "http response status must be an integer from 100 to 999",
    );
  }
  if (response.headers !== undefined) {
    for (const [name, value] of Object.entries(response.headers)) {
      if (typeof value !== "string") {
        throw new TypeError(`http response header '${name}' must be a string`);
      }
    }
  }
  if (response.body != null && !Buffer.isBuffer(response.body)) {
    throw new TypeError(
      "http response body must be a Buffer, null, or undefined",
    );
  }
  return {
    status: response.status,
    headers: response.headers,
    body: response.body ?? null,
  };
}

function resolveHttpHandler(
  http: HttpHandlerConfig | undefined,
): HttpHandler | undefined {
  if (http === undefined) return undefined;
  if (http === true) return defaultHttpHandler;
  if (typeof http !== "function") {
    throw new TypeError("http must be a function, true, or undefined");
  }
  return http;
}

// ---------------------------------------------------------------------------
// Top-level template helper
// ---------------------------------------------------------------------------

export async function buildTemplate(
  runtime: RuntimeName,
  options?: TemplateOptions,
): Promise<SandboxTemplate> {
  const context = new SandboxContext();
  return await context.compileTemplate(runtime, options);
}

// ---------------------------------------------------------------------------
// SandboxContext
// ---------------------------------------------------------------------------

export class SandboxContext {
  private _core: InstanceType<typeof ContextCore>;

  constructor() {
    this._core = new ContextCore();
  }

  async compileTemplate(
    runtime: RuntimeName,
    options?: TemplateOptions,
  ): Promise<SandboxTemplate> {
    validateOptionKeys(options, TEMPLATE_OPTION_KEYS, "template");
    let runtimePath = options?.runtimePath;
    let runtimeLibDir = options?.runtimeLibDir;
    if (!runtimePath) {
      const resolved = await resolveRuntime(runtime, options?.version);
      runtimePath = resolved.runtimePath;
      runtimeLibDir ??= resolved.runtimeLibDir;
    }

    const patch: Record<string, unknown> = {};
    if (options?.cacheDir !== undefined) patch.cache_dir = options.cacheDir;
    if (options?.maxMemory !== undefined) patch.max_memory = options.maxMemory;
    if (options?.prelude !== undefined) patch.prelude = options.prelude;
    if (runtimeLibDir !== undefined) patch.runtime_lib_dir = runtimeLibDir;
    if (options?.mounts !== undefined)
      patch.mounts = encodeMounts(options.mounts);
    if (options?.env !== undefined) patch.env = options.env;

    if (Object.keys(patch).length > 0) {
      this._core.configure(patch);
    }

    await this._core.initializeTemplate(runtimePath, runtime);
    return new SandboxTemplate(this._core);
  }

  close(): void {
    this._core.close();
  }
}

// ---------------------------------------------------------------------------
// SandboxTemplate
// ---------------------------------------------------------------------------

export class SandboxTemplate {
  /** @internal */
  constructor(private _core: InstanceType<typeof ContextCore>) {}

  async create(options?: SandboxOptions): Promise<Sandbox> {
    validateOptionKeys(options, SANDBOX_OPTION_KEYS, "sandbox");
    const core: InstanceType<typeof SandboxCore> =
      await this._core.instantiate();
    const sandbox = new Sandbox(core);

    const configPatch: Record<string, unknown> = {};
    if (options?.maxMemory !== undefined)
      configPatch.max_memory = options.maxMemory;
    if (options?.mounts !== undefined)
      configPatch.mounts = encodeMounts(options.mounts);
    if (options?.env !== undefined) configPatch.env = options.env;
    if (Object.keys(configPatch).length > 0) {
      core.configure(configPatch);
    }

    if (options?.hostcalls) {
      validateHostcalls(options.hostcalls);
      sandbox._setHostcalls(options.hostcalls);
    }
    const http = resolveHttpHandler(options?.http);
    if (http) {
      sandbox._setHttpHandler(http);
    }

    return sandbox;
  }
}

// ---------------------------------------------------------------------------
// Sandbox
// ---------------------------------------------------------------------------

export class Sandbox {
  private _core: InstanceType<typeof SandboxCore>;
  private _busy = false;

  /** @internal */
  constructor(core: InstanceType<typeof SandboxCore>) {
    this._core = core;
  }

  private _beginOperation(): void {
    if (this._busy) throw new Error("sandbox is busy");
    this._busy = true;
  }

  private _endOperation(): void {
    this._busy = false;
  }

  /** @internal */
  _setHostcalls(hostcalls: Hostcalls): void {
    this._core.setHostcallHandler(
      async (...raw: unknown[]): Promise<Buffer> => {
        const [callType, payloadBuffer] = unpackTuple<NativeHostcallArgs>(raw);
        const handler = hostcalls[callType];
        if (!handler) throw new Error(`unsupported hostcall: ${callType}`);
        const payload = decode(payloadBuffer) as JsonValue;
        const result = await handler(payload);
        return Buffer.from(encode(result));
      },
    );
  }

  /** @internal */
  _setHttpHandler(handler: (req: HttpRequest) => Promise<HttpResponse>): void {
    this._core.setHttpHandler(
      async (...raw: unknown[]): Promise<NativeHttpResponse> => {
        const [method, url, headersBuffer, body] =
          unpackTuple<NativeHttpArgs>(raw);
        const headers = JSON.parse(headersBuffer.toString()) as Record<
          string,
          string
        >;
        const req: HttpRequest = { method, url, headers, body };
        const resp = await handler(req);
        return normalizeHttpResponse(resp);
      },
    );
  }

  async start(): Promise<void> {
    await this._core.start();
  }

  async loadScript(code: string): Promise<void> {
    this._beginOperation();
    try {
      await this._core.loadScript(code);
    } finally {
      this._endOperation();
    }
  }

  async run(
    name: string,
    args?: RunArg[],
    kwargs?: RunKwargs,
  ): Promise<JsonValue | null> {
    this._beginOperation();
    try {
      const encoded = encodeArgs(mergeRunArgs(args, kwargs));
      const result = await executeRun(this._core, name, encoded);
      return result.finalJson !== undefined
        ? (JSON.parse(result.finalJson) as JsonValue)
        : null;
    } finally {
      this._endOperation();
    }
  }

  async *runStream(
    name: string,
    args?: RunArg[],
    kwargs?: RunKwargs,
  ): AsyncGenerator<Event, void, undefined> {
    this._beginOperation();
    const queue: Event[] = [];
    let queueHead = 0;
    const emitted = new Map<string, number>();
    let resolve: (() => void) | null = null;
    let done = false;
    let acceptingEvents = true;
    let runResult: NativeRunResult | null = null;
    let runError: unknown = null;
    const activeStreams: NativeStreamHandle[] = [];

    const wake = (): void => {
      if (resolve) {
        resolve();
        resolve = null;
      }
    };

    const pushEvent = (event: Event): void => {
      queue.push(event);
      const key = eventKey(event);
      emitted.set(key, (emitted.get(key) ?? 0) + 1);
      wake();
    };

    const runPromise = Promise.resolve()
      .then(() => {
        const encoded = encodeArgs(mergeRunArgs(args, kwargs));
        activeStreams.push(...encoded.streams);
        this._core.setCallback((...raw: unknown[]) => {
          const [kind, data] = unpackTuple<NativeCallbackArgs>(raw);
          const event = parseEvent(kind, data);
          if (acceptingEvents && event) pushEvent(event);
        });
        return executeRun(this._core, name, encoded);
      })
      .then((result: NativeRunResult) => {
        runResult = result;
      })
      .catch((err: unknown) => {
        runError = err;
      })
      .finally(() => {
        setImmediate(() => {
          done = true;
          wake();
        });
      });

    try {
      while (true) {
        while (queueHead < queue.length) {
          // biome-ignore lint/style/noNonNullAssertion: bounds checked above
          yield queue[queueHead++]!;
        }
        if (queueHead > 0) {
          queue.length = 0;
          queueHead = 0;
        }
        if (done) break;
        await new Promise<void>((r) => {
          resolve = r;
        });
      }
      await runPromise;
      if (runResult) {
        const remaining = new Map(emitted);
        for (const event of resultToEvents(runResult)) {
          const key = eventKey(event);
          const count = remaining.get(key) ?? 0;
          if (count > 0) {
            remaining.set(key, count - 1);
            continue;
          }
          yield event;
        }
      }
      if (runError) throw runError;
    } finally {
      acceptingEvents = false;
      for (const stream of activeStreams) stream.end();
      try {
        this._core.setCallback(null);
      } finally {
        if (done) {
          this._endOperation();
        } else {
          void runPromise.then(() => this._endOperation());
        }
      }
    }
  }

  close(): void {
    this._core.close();
  }

  async [Symbol.asyncDispose](): Promise<void> {
    this.close();
  }
}
