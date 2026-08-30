# Quick Start

This page shows the fastest host-side path from Python or Node.js.

## What Runs Where

- Host code runs in your app and uses the `isola` or `isola-core` SDK to build
  templates, create sandboxes, and configure policy.
- Guest code runs inside the sandbox and uses Python packages such as `httpx2`
  and `sandbox.asyncio`, or JavaScript globals like `hostcall(...)` and
  `fetch(...)`.
- For guest-side APIs, see [Python Guest API](python-guest-api.md) and
  [JavaScript Guest API](javascript-guest-api.md).

## Python SDK

Install the SDK:

```bash
pip install isola
```

Run a small Python guest:

```python
import asyncio

from isola import build_template


async def main() -> None:
    template = await build_template("python")

    async with template.create() as sandbox:
        await sandbox.load_script(
            "def add(a, b):\n"
            "    return a + b\n"
        )
        print(await sandbox.run("add", 1, 2))


asyncio.run(main())
```

Expected output:

```text
3
```

Use `build_template("js")` instead to run a JavaScript guest.

If `runtime_path` is omitted, the SDK downloads and caches the runtime on first
use. For `hostcalls`, mounts, environment variables, and HTTP policy, see
[Python Host API](python-api.md).

## JavaScript / TypeScript SDK

Install the SDK:

```bash
npm install isola-core
```

Run a small Python guest from Node.js:

```typescript
import { buildTemplate } from "isola-core";

const template = await buildTemplate("python");
const sandbox = await template.create();

try {
  await sandbox.start();
  await sandbox.loadScript("def add(a, b):\n    return a + b\n");
  console.log(await sandbox.run("add", [1, 2]));
} finally {
  sandbox.close();
}
```

Expected output:

```text
3
```

Use `buildTemplate("js")` instead to run a JavaScript guest.

If `runtimePath` is omitted, the SDK downloads and caches the runtime on first
use. For `hostcalls`, mounts, environment variables, and HTTP policy, see
[Node.js Host API](nodejs-api.md).
