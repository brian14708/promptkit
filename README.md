# Isola

[![crates.io](https://img.shields.io/crates/v/isola?logo=rust)](https://crates.io/crates/isola)
[![PyPI](https://img.shields.io/pypi/v/isola?logo=python&logoColor=white)](https://pypi.org/project/isola/)
[![npm](https://img.shields.io/npm/v/isola-core?logo=npm)](https://www.npmjs.com/package/isola-core)
[![License](https://img.shields.io/badge/license-Apache%202.0-2563eb)](LICENSE)

Isola runs untrusted Python and JavaScript inside reusable WebAssembly
sandboxes, with host SDKs for Python and Node.js.

It is for cases where embedding an interpreter feels too open, but starting a
container or microVM for every execution feels too heavy. It compiles a
reusable sandbox template once, then instantiates isolated sandboxes with
explicit policy around memory, filesystem mounts, environment variables,
outbound HTTP, and host callbacks.

## Highlights

- Raw-source execution at runtime for Python and JavaScript guests, without a
  per-script build step
- Reusable sandbox templates that amortize startup work across many isolated
  instances
- A CPython-based Python guest built for WASI, with native `async`/`await`
  support inside the sandbox
- Python and Node.js host SDKs with explicit capabilities for mounts, env,
  HTTP, and hostcalls

## Quick Start

Install the Python SDK:

```bash
pip install isola
```

If you are embedding from Node.js instead, install `isola-core`.

```python
import asyncio

from isola import build_template


async def main() -> None:
    # First run downloads and precompiles the runtime.
    template = await build_template("python")

    async with template.create(http=True) as sandbox:
        await sandbox.load_script(
            "import httpx2\n"
            "\n"
            "async def main(url):\n"
            "    async with httpx2.AsyncClient() as client:\n"
            "        resp = await client.get(url)\n"
            "        return resp.json()\n"
        )
        print(await sandbox.run("main", "https://httpbin.org/get"))


asyncio.run(main())
```

## Potential Good Fits

- AI code execution, where an agent writes short Python or JavaScript helpers
  and the host decides what external effects are allowed
- Multi-tenant plugin systems that want per-request sandboxes without starting a
  container for every call
- User-authored automation or ETL steps that need streaming outputs and
  policy-wrapped access to internal services
- Internal extension points where teams want Python or JavaScript ergonomics
  without giving scripts full host access

Isola is not a full Linux environment or a replacement for containers or
microVMs. If you need arbitrary native extensions, subprocesses, or
infrastructure-level isolation, use something stronger.
