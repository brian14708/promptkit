use std::{sync::Arc, time::Duration};

use anyhow::{Context, Result};
use isola::{
    host::{OutputEvent, OutputTarget},
    sandbox::{Arg, CallOutput, Error as IsolaError, FsPerms, Sandbox, SandboxOptions, args},
};
use parking_lot::Mutex;
use tempfile::tempdir;

use super::common::{TestHost, build_module, build_module_with_max_memory};

const CAP_NEIGHBORHOOD_BYTES: usize = 1024 * 1024;
const MEMORY_CAP_BYTES: usize = 64 * 1024 * 1024;
const LARGE_STDOUT_BYTES: usize = 256 * 1024;

struct CollectLogsSink {
    logs: Arc<Mutex<Vec<(String, String)>>>,
}

impl CollectLogsSink {
    const fn new(logs: Arc<Mutex<Vec<(String, String)>>>) -> Self {
        Self { logs }
    }

    fn into_target(self) -> OutputTarget {
        OutputTarget::synchronous(move |event| {
            if let OutputEvent::Log { level, message, .. } = event {
                self.logs.lock().push((level.as_str().to_string(), message));
            }
            Ok(())
        })
    }
}

async fn call_with_timeout<I>(
    sandbox: &mut Sandbox<TestHost>,
    function: &str,
    args: I,
    timeout: Duration,
) -> std::result::Result<CallOutput, IsolaError>
where
    I: IntoIterator<Item = Arg>,
{
    tokio::time::timeout(timeout, sandbox.call(function, args))
        .await
        .unwrap_or_else(|_| {
            Err(IsolaError::Other(
                anyhow::anyhow!("sandbox call timed out after {}ms", timeout.as_millis()).into(),
            ))
        })
}

#[tokio::test]
#[cfg_attr(debug_assertions, ignore = "integration tests run in release mode")]
async fn integration_python_eval_and_call_roundtrip() -> Result<()> {
    let Some(module) = build_module().await? else {
        return Ok(());
    };
    let mut sandbox = module
        .instantiate(TestHost::default(), SandboxOptions::default())
        .await
        .context("failed to instantiate sandbox")?;

    sandbox
        .eval_script(
            "def main():\n\tprint('trace-print')\n\treturn 42",
            OutputTarget::discard(),
        )
        .await
        .context("failed to evaluate script")?;

    let output = call_with_timeout(&mut sandbox, "main", [], Duration::from_secs(2))
        .await
        .context("failed to call function")?;

    assert!(output.items.is_empty(), "expected no partial outputs");
    let value: i64 = output
        .result
        .as_ref()
        .context("expected exactly one end output")?
        .to_serde()
        .context("failed to decode end output")?;
    assert_eq!(value, 42);

    Ok(())
}

#[tokio::test]
#[cfg_attr(debug_assertions, ignore = "integration tests run in release mode")]
async fn integration_python_final_turn_callbacks_run() -> Result<()> {
    let Some(module) = build_module().await? else {
        return Ok(());
    };
    let mut sandbox = module
        .instantiate(TestHost::default(), SandboxOptions::default())
        .await
        .context("failed to instantiate sandbox")?;

    sandbox
        .eval_script(
            "import asyncio\n\
             seen = []\n\
             async def main():\n\
             \tasyncio.get_running_loop().call_soon(seen.append, 'ran')\n\
             \treturn 'done'\n\
             def observe():\n\
             \treturn seen",
            OutputTarget::discard(),
        )
        .await
        .context("failed to evaluate final-turn callback script")?;

    call_with_timeout(&mut sandbox, "main", [], Duration::from_secs(2))
        .await
        .context("failed to run final-turn callback function")?;
    let output = call_with_timeout(&mut sandbox, "observe", [], Duration::from_secs(2))
        .await
        .context("failed to observe final-turn callback")?;
    let seen: Vec<String> = output
        .result
        .as_ref()
        .context("expected end output")?
        .to_serde()
        .context("failed to decode final-turn callbacks")?;

    assert_eq!(seen, vec!["ran"]);

    Ok(())
}

#[tokio::test]
#[cfg_attr(debug_assertions, ignore = "integration tests run in release mode")]
async fn integration_python_self_rescheduling_callback_does_not_starve_timer() -> Result<()> {
    let Some(module) = build_module().await? else {
        return Ok(());
    };
    let mut sandbox = module
        .instantiate(TestHost::default(), SandboxOptions::default())
        .await
        .context("failed to instantiate sandbox")?;

    sandbox
        .eval_script(
            "import asyncio\n\
             async def main():\n\
             \tloop = asyncio.get_running_loop()\n\
             \tdone = loop.create_future()\n\
             \tturns = 0\n\
             \tdef reschedule():\n\
             \t\tnonlocal turns\n\
             \t\tturns += 1\n\
             \t\tloop.call_soon(reschedule)\n\
             \tloop.call_soon(reschedule)\n\
             \tloop.call_later(0.01, lambda: None)\n\
             \tloop.call_later(0.05, done.set_result, None)\n\
             \tawait done\n\
             \treturn turns",
            OutputTarget::discard(),
        )
        .await
        .context("failed to evaluate callback fairness script")?;

    let output = call_with_timeout(&mut sandbox, "main", [], Duration::from_secs(2))
        .await
        .context("self-rescheduling callback starved the timer")?;
    let turns: i64 = output
        .result
        .as_ref()
        .context("expected end output")?
        .to_serde()
        .context("failed to decode callback count")?;

    assert!(turns > 0);

    Ok(())
}

#[tokio::test]
#[cfg_attr(debug_assertions, ignore = "integration tests run in release mode")]
async fn integration_python_output_target_does_not_retain_refs() -> Result<()> {
    let Some(module) = build_module().await? else {
        return Ok(());
    };
    let mut sandbox = module
        .instantiate(TestHost::default(), SandboxOptions::default())
        .await
        .context("failed to instantiate sandbox")?;

    sandbox
        .eval_script("def main():\n\treturn 42", OutputTarget::discard())
        .await
        .context("failed to evaluate script")?;

    let marker = Arc::new(());
    let initial = Arc::strong_count(&marker);
    assert_eq!(initial, 1, "unexpected initial marker refcount");

    let retained = Arc::clone(&marker);
    let target = OutputTarget::synchronous(move |_event| {
        let _count = Arc::strong_count(&retained);
        Ok(())
    });

    sandbox
        .call_with_sink("main", [], target)
        .await
        .context("failed to call function with target")?;
    assert_eq!(
        Arc::strong_count(&marker),
        initial,
        "target capture was retained after call_with_sink",
    );

    let retained = Arc::clone(&marker);
    let target = OutputTarget::synchronous(move |_event| {
        let _count = Arc::strong_count(&retained);
        Ok(())
    });
    sandbox
        .call_with_sink("main", [], target)
        .await
        .context("failed to call function with target on second call")?;
    assert_eq!(
        Arc::strong_count(&marker),
        initial,
        "target capture was retained after repeated call_with_sink",
    );

    Ok(())
}

#[tokio::test]
#[cfg_attr(debug_assertions, ignore = "integration tests run in release mode")]
async fn integration_python_call_with_bounded_output_channel() -> Result<()> {
    let Some(module) = build_module().await? else {
        return Ok(());
    };
    let mut sandbox = module
        .instantiate(TestHost::default(), SandboxOptions::default())
        .await
        .context("failed to instantiate sandbox")?;

    sandbox
        .eval_script(
            "def main():\n\tyield 1\n\treturn 2",
            OutputTarget::discard(),
        )
        .await
        .context("failed to evaluate script")?;

    let (sender, mut receiver) = tokio::sync::mpsc::channel(2);
    sandbox
        .call_with_sink("main", [], sender)
        .await
        .context("failed to call function with channel target")?;

    assert!(matches!(receiver.recv().await, Some(OutputEvent::Item(_))));
    assert!(matches!(
        receiver.recv().await,
        Some(OutputEvent::Complete(_))
    ));
    assert!(
        receiver.recv().await.is_none(),
        "output target was retained"
    );

    Ok(())
}

#[tokio::test]
#[cfg_attr(debug_assertions, ignore = "integration tests run in release mode")]
async fn integration_python_streaming_output() -> Result<()> {
    let Some(module) = build_module().await? else {
        return Ok(());
    };
    let mut sandbox = module
        .instantiate(TestHost::default(), SandboxOptions::default())
        .await
        .context("failed to instantiate sandbox")?;

    sandbox
        .eval_script(
            "def main():\n\tfor i in range(3):\n\t\tyield i",
            OutputTarget::discard(),
        )
        .await
        .context("failed to evaluate streaming script")?;

    let output = call_with_timeout(&mut sandbox, "main", [], Duration::from_secs(2))
        .await
        .context("failed to call streaming function")?;

    assert_eq!(output.items.len(), 3, "expected three partial outputs");
    let mut values = Vec::with_capacity(output.items.len());
    for item in &output.items {
        values.push(
            item.to_serde::<i64>()
                .context("failed to decode partial output")?,
        );
    }
    assert_eq!(values, vec![0, 1, 2]);

    assert!(output.result.is_none(), "expected null end output");

    Ok(())
}

#[tokio::test]
#[cfg_attr(debug_assertions, ignore = "integration tests run in release mode")]
async fn integration_python_failed_emit_does_not_corrupt_next_output() -> Result<()> {
    let Some(module) = build_module().await? else {
        return Ok(());
    };
    let mut sandbox = module
        .instantiate(TestHost::default(), SandboxOptions::default())
        .await
        .context("failed to instantiate sandbox")?;

    sandbox
        .eval_script(
            "import _isola_sys\n\
             def main():\n\
             \ttry:\n\
             \t\t_isola_sys.emit([\"x\" * 2048, object()])\n\
             \texcept ValueError:\n\
             \t\tpass\n\
             \t_isola_sys.emit(\"valid\")\n\
             \treturn \"done\"",
            OutputTarget::discard(),
        )
        .await
        .context("failed to evaluate failed emit script")?;

    let output = call_with_timeout(&mut sandbox, "main", [], Duration::from_secs(5))
        .await
        .context("failed to call failed emit function")?;
    assert_eq!(output.items.len(), 1, "failed emit should not reach sink");
    let item: String = output.items[0]
        .to_serde()
        .context("failed to decode valid partial output")?;
    assert_eq!(item, "valid");
    let result: String = output
        .result
        .as_ref()
        .context("expected end output")?
        .to_serde()
        .context("failed to decode final output")?;
    assert_eq!(result, "done");

    Ok(())
}

#[tokio::test]
#[cfg_attr(debug_assertions, ignore = "integration tests run in release mode")]
async fn integration_python_eval_script_logs_to_sink() -> Result<()> {
    let Some(module) = build_module().await? else {
        return Ok(());
    };
    let mut sandbox = module
        .instantiate(TestHost::default(), SandboxOptions::default())
        .await
        .context("failed to instantiate sandbox")?;

    let logs = Arc::new(Mutex::new(Vec::new()));
    let sink = CollectLogsSink::new(logs.clone());
    match tokio::time::timeout(
        Duration::from_secs(2),
        sandbox.eval_script(
            "print('eval-stdout')\nimport sandbox.logging\nsandbox.logging.info('eval-log')",
            sink.into_target(),
        ),
    )
    .await
    {
        Ok(result) => result.context("failed to evaluate script")?,
        Err(_) => {
            return Err(anyhow::anyhow!("sandbox eval timed out after {}ms", 2_000));
        }
    }
    {
        let logs = logs.lock();

        assert!(
            logs.iter()
                .any(|(context, message)| context == "stdout" && message.contains("eval-stdout")),
            "expected eval stdout log in sink, logs: {:?}",
            *logs
        );
        assert!(
            logs.iter()
                .any(|(context, message)| context == "info" && message.contains("eval-log")),
            "expected eval logging event in sink, logs: {:?}",
            *logs
        );
        drop(logs);
    }

    Ok(())
}

#[tokio::test]
#[cfg_attr(debug_assertions, ignore = "integration tests run in release mode")]
async fn integration_python_large_stdout_output_is_not_truncated() -> Result<()> {
    let Some(module) = build_module().await? else {
        return Ok(());
    };
    let mut sandbox = module
        .instantiate(TestHost::default(), SandboxOptions::default())
        .await
        .context("failed to instantiate sandbox")?;

    sandbox
        .eval_script(
            format!(
                "def main():\n\
                 \tpayload = 'x' * {LARGE_STDOUT_BYTES}\n\
                 \tprint(payload, end='')\n\
                 \treturn len(payload)"
            ),
            OutputTarget::discard(),
        )
        .await
        .context("failed to evaluate large stdout script")?;

    let output = call_with_timeout(&mut sandbox, "main", [], Duration::from_secs(10))
        .await
        .context("failed to call large stdout function")?;

    let emitted_len: usize = output
        .result
        .as_ref()
        .context("expected exactly one end output")?
        .to_serde()
        .context("failed to decode end output")?;
    assert_eq!(emitted_len, LARGE_STDOUT_BYTES);

    Ok(())
}

#[tokio::test]
#[cfg_attr(debug_assertions, ignore = "integration tests run in release mode")]
async fn integration_python_argument_cbor_path() -> Result<()> {
    let Some(module) = build_module().await? else {
        return Ok(());
    };
    let mut sandbox = module
        .instantiate(TestHost::default(), SandboxOptions::default())
        .await
        .context("failed to instantiate sandbox")?;

    sandbox
        .eval_script(
            "def main(i, s):\n\treturn (i + 1, s.upper())",
            OutputTarget::discard(),
        )
        .await
        .context("failed to evaluate argument script")?;

    let args = args![41_i64, s = "hello"]?;
    let output = call_with_timeout(&mut sandbox, "main", args, Duration::from_secs(2))
        .await
        .context("failed to call argument function")?;

    assert!(output.items.is_empty(), "expected no partial outputs");
    let value: (i64, String) = output
        .result
        .as_ref()
        .context("expected exactly one end output")?
        .to_serde()
        .context("failed to decode argument result")?;
    assert_eq!(value, (42, "HELLO".to_string()));
    Ok(())
}

#[tokio::test]
#[cfg_attr(debug_assertions, ignore = "integration tests run in release mode")]
async fn integration_python_numpy_typed_array_cbor_path() -> Result<()> {
    let Some(module) = build_module().await? else {
        return Ok(());
    };
    let mut sandbox = module
        .instantiate(TestHost::default(), SandboxOptions::default())
        .await?;
    sandbox
        .eval_script(
            "def main():\n\timport numpy as np\n\treturn np.array([1.5, -2.25], dtype='<f4')",
            OutputTarget::discard(),
        )
        .await?;
    let output = call_with_timeout(&mut sandbox, "main", [], Duration::from_secs(5)).await?;
    let value = output.result.context("expected typed-array result")?;
    let mut decoder = minicbor::Decoder::new(value.as_cbor());
    assert_eq!(decoder.tag()?.as_u64(), 84);
    assert_eq!(decoder.bytes()?, &[0, 0, 192, 63, 0, 0, 16, 192]);
    assert_eq!(value.to_json_str()?, r#""AADAPwAAEMA=""#);
    Ok(())
}

#[tokio::test]
#[cfg_attr(debug_assertions, ignore = "integration tests run in release mode")]
async fn integration_python_reinstantiate_smoke() -> Result<()> {
    let Some(module) = build_module().await? else {
        return Ok(());
    };

    for expected in [7_i64, 11_i64] {
        let mut sandbox = module
            .instantiate(TestHost::default(), SandboxOptions::default())
            .await
            .context("failed to instantiate sandbox")?;

        sandbox
            .eval_script(
                format!("def main():\n\treturn {expected}"),
                OutputTarget::discard(),
            )
            .await
            .context("failed to evaluate script")?;

        let output = call_with_timeout(&mut sandbox, "main", [], Duration::from_secs(2))
            .await
            .context("failed to call function")?;
        let value: i64 = output
            .result
            .as_ref()
            .context("expected exactly one end output")?
            .to_serde()
            .context("failed to decode roundtrip output")?;
        assert_eq!(value, expected);
    }

    Ok(())
}

#[tokio::test]
#[cfg_attr(debug_assertions, ignore = "integration tests run in release mode")]
async fn integration_python_guest_exception_surface() -> Result<()> {
    let Some(module) = build_module().await? else {
        return Ok(());
    };
    let mut sandbox = module
        .instantiate(TestHost::default(), SandboxOptions::default())
        .await
        .context("failed to instantiate sandbox")?;

    sandbox
        .eval_script(
            "def main():\n\traise RuntimeError(\"boom\")",
            OutputTarget::discard(),
        )
        .await
        .context("failed to evaluate exception script")?;

    let err = call_with_timeout(&mut sandbox, "main", [], Duration::from_secs(2))
        .await
        .expect_err("expected exception from guest function");
    let IsolaError::UserCode { message } = err else {
        panic!("expected guest error, got {err:?}");
    };
    assert!(
        message.contains("boom"),
        "unexpected error message: {message}",
    );

    Ok(())
}

#[tokio::test]
#[cfg_attr(debug_assertions, ignore = "integration tests run in release mode")]
async fn integration_python_state_persists_within_sandbox() -> Result<()> {
    let Some(module) = build_module().await? else {
        return Ok(());
    };
    let mut sandbox = module
        .instantiate(TestHost::default(), SandboxOptions::default())
        .await
        .context("failed to instantiate sandbox")?;

    sandbox
        .eval_script(
            "counter = 0\n\
             def main():\n\
             \tglobal counter\n\
             \tcounter += 1\n\
             \treturn counter",
            OutputTarget::discard(),
        )
        .await
        .context("failed to evaluate stateful script")?;

    let first = call_with_timeout(&mut sandbox, "main", [], Duration::from_secs(2))
        .await
        .context("failed first stateful call")?;
    let second = call_with_timeout(&mut sandbox, "main", [], Duration::from_secs(2))
        .await
        .context("failed second stateful call")?;

    let first_v: i64 = first
        .result
        .as_ref()
        .context("expected exactly one first end output")?
        .to_serde()
        .context("failed to decode first value")?;
    let second_v: i64 = second
        .result
        .as_ref()
        .context("expected exactly one second end output")?
        .to_serde()
        .context("failed to decode second value")?;
    assert_eq!(first_v, 1);
    assert_eq!(second_v, 2);

    Ok(())
}

#[tokio::test]
#[cfg_attr(debug_assertions, ignore = "integration tests run in release mode")]
async fn integration_python_call_timeout() -> Result<()> {
    let Some(module) = build_module().await? else {
        return Ok(());
    };
    let mut sandbox = module
        .instantiate(TestHost::default(), SandboxOptions::default())
        .await
        .context("failed to instantiate sandbox")?;

    sandbox
        .eval_script(
            "def main():\n\twhile True:\n\t\tpass",
            OutputTarget::discard(),
        )
        .await
        .context("failed to evaluate timeout script")?;

    let err = call_with_timeout(&mut sandbox, "main", [], Duration::from_millis(1))
        .await
        .expect_err("expected timeout while executing guest function");
    let IsolaError::Other(cause) = err else {
        panic!("expected runtime timeout error, got {err:?}");
    };
    let message = cause.to_string().to_ascii_lowercase();
    assert!(
        message.contains("timeout") || message.contains("timed out"),
        "unexpected timeout error message: {cause}"
    );

    Ok(())
}

#[tokio::test]
#[cfg_attr(debug_assertions, ignore = "integration tests run in release mode")]
async fn integration_python_asyncio_sleep_respects_delay() -> Result<()> {
    let Some(module) = build_module().await? else {
        return Ok(());
    };
    let mut sandbox = module
        .instantiate(TestHost::default(), SandboxOptions::default())
        .await
        .context("failed to instantiate sandbox")?;

    let script = r"
import asyncio
import sandbox.asyncio

async def main():
    loop = asyncio.get_running_loop()
    start = loop.time()
    fut = loop.create_future()
    loop.call_later(0.05, fut.set_result, None)
    await fut
    return loop.time() - start
";
    sandbox
        .eval_script(script, OutputTarget::discard())
        .await
        .context("failed to evaluate asyncio sleep script")?;

    let started = std::time::Instant::now();
    let output = call_with_timeout(&mut sandbox, "main", [], Duration::from_secs(5))
        .await
        .context("failed to call asyncio sleep function")?;
    let host_elapsed = started.elapsed();

    assert!(output.items.is_empty(), "expected no partial outputs");
    let elapsed: f64 = output
        .result
        .as_ref()
        .context("expected end output")?
        .to_serde()
        .context("failed to decode elapsed time")?;
    assert!(
        elapsed >= 0.045 || host_elapsed >= Duration::from_millis(45),
        "sleep resolved too early, guest_elapsed={elapsed}, host_elapsed={host_elapsed:?}"
    );

    Ok(())
}

#[tokio::test]
#[cfg_attr(debug_assertions, ignore = "integration tests run in release mode")]
async fn integration_python_infinite_sleep_remains_pending() -> Result<()> {
    let Some(module) = build_module().await? else {
        return Ok(());
    };
    let mut sandbox = module
        .instantiate(TestHost::default(), SandboxOptions::default())
        .await
        .context("failed to instantiate sandbox")?;

    sandbox
        .eval_script(
            "import asyncio\n\
             async def main():\n\
             \ttry:\n\
             \t\tawait asyncio.wait_for(asyncio.sleep(float('inf')), 0.02)\n\
             \texcept TimeoutError:\n\
             \t\treturn True\n\
             \treturn False",
            OutputTarget::discard(),
        )
        .await
        .context("failed to evaluate infinite sleep script")?;

    let output = call_with_timeout(&mut sandbox, "main", [], Duration::from_secs(2))
        .await
        .context("infinite sleep did not respect the finite timeout")?;
    let timed_out: bool = output
        .result
        .as_ref()
        .context("expected end output")?
        .to_serde()
        .context("failed to decode infinite sleep result")?;

    assert!(timed_out, "positive-infinite sleep completed immediately");

    Ok(())
}

#[tokio::test]
#[cfg_attr(debug_assertions, ignore = "integration tests run in release mode")]
async fn integration_python_runner_finalizes_retained_async_generators() -> Result<()> {
    let Some(module) = build_module().await? else {
        return Ok(());
    };
    let mut sandbox = module
        .instantiate(TestHost::default(), SandboxOptions::default())
        .await
        .context("failed to instantiate sandbox")?;

    let script = r#"
finalized = []
retained = None

async def values(name):
    try:
        yield 1
    finally:
        finalized.append(name)

async def start():
    global retained
    retained = values("retained")
    await anext(retained)
    released = values("released")
    await anext(released)

def observe():
    return sorted(finalized)
"#;
    sandbox
        .eval_script(script, OutputTarget::discard())
        .await
        .context("failed to evaluate async-generator finalization script")?;

    call_with_timeout(&mut sandbox, "start", [], Duration::from_secs(2))
        .await
        .context("failed to start retained async generator")?;
    let output = call_with_timeout(&mut sandbox, "observe", [], Duration::from_secs(2))
        .await
        .context("failed to observe async-generator finalization")?;
    let finalized: Vec<String> = output
        .result
        .as_ref()
        .context("expected end output")?
        .to_serde()
        .context("failed to decode async-generator finalization result")?;

    assert_eq!(finalized, vec!["released", "retained"]);

    Ok(())
}

#[tokio::test]
#[cfg_attr(debug_assertions, ignore = "integration tests run in release mode")]
async fn integration_python_oversized_sleep_is_rejected() -> Result<()> {
    let Some(module) = build_module().await? else {
        return Ok(());
    };
    let mut sandbox = module
        .instantiate(TestHost::default(), SandboxOptions::default())
        .await
        .context("failed to instantiate sandbox")?;

    sandbox
        .eval_script(
            "import _isola_sys\n\
             def main():\n\
             \ttry:\n\
             \t\t_isola_sys.sleep(1e300)\n\
             \texcept OverflowError:\n\
             \t\treturn True\n\
             \treturn False",
            OutputTarget::discard(),
        )
        .await
        .context("failed to evaluate oversized sleep script")?;

    let output = call_with_timeout(&mut sandbox, "main", [], Duration::from_secs(5))
        .await
        .context("failed to call oversized sleep function")?;
    let rejected: bool = output
        .result
        .as_ref()
        .context("expected end output")?
        .to_serde()
        .context("failed to decode oversized sleep result")?;
    assert!(
        rejected,
        "oversized Python sleep should raise OverflowError"
    );

    Ok(())
}

#[tokio::test]
#[cfg_attr(debug_assertions, ignore = "integration tests run in release mode")]
async fn integration_python_memory_limiter_is_enforced() -> Result<()> {
    let Some(module) = build_module_with_max_memory(MEMORY_CAP_BYTES).await? else {
        return Ok(());
    };
    let mut sandbox = module
        .instantiate(TestHost::default(), SandboxOptions::default())
        .await
        .context("failed to instantiate sandbox")?;

    sandbox
        .eval_script(
            "def main():\n\
             \tchunks = []\n\
             \tfor _ in range(1024):\n\
             \t\tchunks.append(bytes(1024 * 1024))\n\
             \treturn len(chunks)",
            OutputTarget::discard(),
        )
        .await
        .context("failed to evaluate memory pressure script")?;

    let memory_before = sandbox.memory_usage();
    let err = call_with_timeout(&mut sandbox, "main", [], Duration::from_secs(10))
        .await
        .expect_err("expected memory limit error while allocating guest memory");
    let memory_after = sandbox.memory_usage();

    let message = match err {
        IsolaError::UserCode { message } => message.to_ascii_lowercase(),
        IsolaError::Wasm(cause) => cause.to_string().to_ascii_lowercase(),
        IsolaError::Io(cause) => cause.to_string().to_ascii_lowercase(),
        IsolaError::Other(cause) => cause.to_string().to_ascii_lowercase(),
    };
    assert!(
        message.contains("memory")
            || message.contains("grow")
            || message.contains("alloc")
            || message.contains("oom"),
        "unexpected memory limit error message: {message}",
    );

    assert!(
        memory_after >= memory_before,
        "expected memory usage to grow during allocation, before={memory_before}, after={memory_after}",
    );
    assert!(
        memory_after <= MEMORY_CAP_BYTES,
        "memory usage exceeded configured cap: used={memory_after}, cap={MEMORY_CAP_BYTES}",
    );
    assert!(
        memory_after >= MEMORY_CAP_BYTES.saturating_sub(CAP_NEIGHBORHOOD_BYTES),
        "expected usage to reach memory cap neighborhood, used={memory_after}, cap={MEMORY_CAP_BYTES}",
    );

    Ok(())
}

#[tokio::test]
#[cfg_attr(debug_assertions, ignore = "integration tests run in release mode")]
async fn integration_python_writable_directory_mapping_filesystem_roundtrip() -> Result<()> {
    let temp = tempdir().context("failed to create temp directory")?;
    let mapped_dir = temp.path().to_path_buf();

    let Some(module) = build_module().await? else {
        return Ok(());
    };
    let mut options = SandboxOptions::default();
    options = options.mount(&mapped_dir, "/fs", FsPerms::ReadWrite);
    let mut sandbox = module
        .instantiate(TestHost::default(), options)
        .await
        .context("failed to instantiate sandbox")?;

    sandbox
        .eval_script(
            "def main(text):\n\
             \tpath = '/fs/output.txt'\n\
             \twith open(path, 'w', encoding='utf-8') as fh:\n\
             \t\tfh.write(text)\n\
             \twith open(path, 'r', encoding='utf-8') as fh:\n\
             \t\treturn fh.read()",
            OutputTarget::discard(),
        )
        .await
        .context("failed to evaluate filesystem script")?;

    let args = args!["hello-fs"]?;
    let output = call_with_timeout(&mut sandbox, "main", args, Duration::from_secs(2))
        .await
        .context("failed to call filesystem function")?;

    assert!(output.items.is_empty(), "expected no partial outputs");
    let result: String = output
        .result
        .as_ref()
        .context("expected exactly one end output")?
        .to_serde()
        .context("failed to decode filesystem result")?;
    assert_eq!(result, "hello-fs");

    let host_file = mapped_dir.join("output.txt");
    let host_contents = std::fs::read_to_string(&host_file).with_context(|| {
        format!(
            "failed to read mapped host file after guest write: {}",
            host_file.display()
        )
    })?;
    assert_eq!(host_contents, "hello-fs");

    Ok(())
}

const ZSTD_SCRIPT: &str = r#"
def main():
    from compression import zstd

    payload = b"isola-zstd-roundtrip " * 256
    blob = zstd.compress(payload, level=10)
    assert zstd.decompress(blob) == payload, "zstd roundtrip mismatch"
    return len(blob) < len(payload)
"#;

#[tokio::test]
#[cfg_attr(debug_assertions, ignore = "integration tests run in release mode")]
async fn integration_python_stdlib_zstd_roundtrip() -> Result<()> {
    let Some(module) = build_module().await? else {
        return Ok(());
    };
    let mut sandbox = module
        .instantiate(TestHost::default(), SandboxOptions::default())
        .await
        .context("failed to instantiate sandbox")?;

    sandbox
        .eval_script(ZSTD_SCRIPT, OutputTarget::discard())
        .await
        .context("failed to evaluate zstd script")?;

    let output = call_with_timeout(&mut sandbox, "main", [], Duration::from_secs(5))
        .await
        .context("failed to call zstd function")?;

    assert!(output.items.is_empty(), "expected no partial outputs");
    let compressed_smaller: bool = output
        .result
        .as_ref()
        .context("expected exactly one end output")?
        .to_serde()
        .context("failed to decode zstd result")?;
    assert!(compressed_smaller, "zstd output was not smaller than input");

    Ok(())
}
