use std::{
    collections::hash_map::DefaultHasher,
    env,
    hash::Hasher,
    path::{Path, PathBuf},
    time::UNIX_EPOCH,
};

use anyhow::{Context, Result, ensure};
use wasmparser::{Parser, Payload};
use xshell::{Cmd, Shell, cmd};

use crate::async_shim::link_library;

mod async_shim;

const TARGET: &str = "wasm32-wasip2";
const COMPONENT_BUILD_INPUTS: &[(&str, &[u8])] = &[
    ("crates/xtask/src/main.rs", include_bytes!("main.rs")),
    (
        "crates/xtask/src/async_shim.rs",
        include_bytes!("async_shim.rs"),
    ),
    ("crates/xtask/Cargo.toml", include_bytes!("../Cargo.toml")),
    ("Cargo.toml", include_bytes!("../../../Cargo.toml")),
    ("Cargo.lock", include_bytes!("../../../Cargo.lock")),
];

struct ComponentLibrary {
    name: String,
    path: PathBuf,
    dlopen: bool,
    async_shim_name: Option<&'static str>,
}

impl ComponentLibrary {
    fn new(
        name: impl Into<String>,
        path: impl AsRef<Path>,
        dlopen: bool,
        async_shim_name: Option<&'static str>,
    ) -> Self {
        Self {
            name: name.into(),
            path: path.as_ref().to_path_buf(),
            dlopen,
            async_shim_name,
        }
    }

    fn load(self) -> Result<LoadedComponentLibrary> {
        let module = std::fs::read(&self.path)
            .with_context(|| format!("failed to read component library {}", self.path.display()))?;
        Ok(LoadedComponentLibrary {
            name: self.name,
            module,
            dlopen: self.dlopen,
            async_shim_name: self.async_shim_name,
        })
    }
}

struct LoadedComponentLibrary {
    name: String,
    module: Vec<u8>,
    dlopen: bool,
    async_shim_name: Option<&'static str>,
}

struct FingerprintHasher {
    first: DefaultHasher,
    second: DefaultHasher,
}

impl FingerprintHasher {
    fn new() -> Self {
        let first = DefaultHasher::new();
        let mut second = DefaultHasher::new();
        second.write_u8(1);
        Self { first, second }
    }

    fn write(&mut self, value: &[u8]) {
        self.first.write(value);
        self.second.write(value);
    }

    fn finish(self) -> String {
        format!("{:016x}{:016x}", self.first.finish(), self.second.finish())
    }
}

fn main() -> Result<()> {
    let workspace_dir = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .parent()
        .unwrap();
    std::env::set_current_dir(workspace_dir)?;

    let task = std::env::args().nth(1);

    let sh = Shell::new()?;
    if let Some(cmd) = task.as_deref() {
        let f = TASKS
            .iter()
            .find_map(|(k, f)| (*k == cmd).then_some(*f))
            .unwrap_or(print_help);
        f(&sh)?;
    } else {
        print_help(&sh)?;
    }
    Ok(())
}

type Task = fn(&Shell) -> Result<()>;
const TASKS: &[(&str, Task)] = &[
    ("build-all", build_all),
    ("build-python", build_python),
    ("build-js", build_js),
];

#[expect(clippy::unnecessary_wraps, reason = "matches Task fn signature")]
fn print_help(_sh: &Shell) -> Result<()> {
    println!("Tasks:");
    for (name, _) in TASKS {
        println!("  - {name}");
    }
    Ok(())
}

fn build_all(sh: &Shell) -> Result<()> {
    build_python(sh)?;
    build_js(sh)?;
    Ok(())
}

/// Rustflags for the guest runtime builds, mirroring componentize-py's
/// `make_runtime`:
///
/// - `--cfg pyo3_disable_reference_pool`: the reference pool is a global mutex
///   for deferring `Py_DECREF` from non-GIL threads; the guest is
///   single-threaded with one interpreter, so the lock is pure overhead.
/// - `-Wl,--skip-wit-component`: the `wasm32-wasip2` target normally emits a
///   *component*; this stops the linker at the core module so
///   `wit_component::Linker` can consume it as a shared library.
/// - `-C link-self-contained=n`: resolve libc from the wasi-sdk sysroot
///   (reached through the clang linker driver) instead of rust's bundled copy,
///   so the runtime shares the dylink libc the other libraries use.
fn wasm_rustflags(wasi_deps_dir: &str) -> String {
    format!(
        "--cfg pyo3_disable_reference_pool \
         -C relocation-model=pic \
         -C link-args=-Wl,--skip-wit-component \
         -C link-arg=-shared -C link-args=-Wl,--allow-undefined \
         -C link-self-contained=n \
         -Lnative={wasi_deps_dir}/lib"
    )
}

/// The `wasm32-wasip2` linker: the wasi-sdk clang driver (which locates the p2
/// sysroot relative to its own binary), same as componentize-py.
fn wasm_linker() -> String {
    let wasi_sdk = env::var("WASI_SDK")
        .expect("WASI_SDK must be set for wasm32 builds (run inside `nix develop`)");
    format!("{wasi_sdk}/bin/clang")
}

/// The env var cargo reads the linker for `TARGET` from, e.g.
/// `CARGO_TARGET_WASM32_WASIP2_LINKER`.
fn linker_env_key() -> String {
    format!(
        "CARGO_TARGET_{}_LINKER",
        TARGET.to_uppercase().replace('-', "_")
    )
}

/// Remove host cargo/rust configuration from the environment of the nested
/// `cargo build` for the wasm guest. xtask is itself launched through cargo, so
/// a stray `CARGO_ENCODED_RUSTFLAGS` in the environment would take precedence
/// over the `RUSTFLAGS` we set for the child and silently drop the pic/shared
/// link flags. `CARGO_HOME` is kept: the child needs it to find vendored
/// dependencies (crane sets it in the Nix build). `CARGO_TARGET_DIR` is
/// deliberately dropped so output always lands in the workspace `target/`
/// directory the callers below read from.
fn scrub_host_build_env(mut cmd: Cmd) -> Cmd {
    for (key, _) in env::vars_os() {
        if key == "CARGO_HOME" {
            continue;
        }
        let key = key.to_string_lossy().into_owned();
        if key.starts_with("RUST") || key.starts_with("CARGO") {
            cmd = cmd.env_remove(key);
        }
    }
    cmd
}

/// Encode rustflags for `CARGO_ENCODED_RUSTFLAGS` (unit-separator delimited).
/// The encoded form takes precedence over every other rustflags source —
/// `RUSTFLAGS`, `[build] rustflags`, and `target.<triple>.rustflags` — so host
/// configuration cannot override the guest link flags.
fn encode_rustflags(rustflags: &str) -> String {
    rustflags
        .split_whitespace()
        .collect::<Vec<_>>()
        .join("\u{1f}")
}

fn build_python(sh: &Shell) -> Result<()> {
    build_guest(
        sh,
        "isola-python-runtime",
        Some(("PYO3_CROSS_PYTHON_VERSION", "3.15")),
        python_libraries,
        Path::new("target/python.wasm"),
        8_388_608,
    )
}

fn build_js(sh: &Shell) -> Result<()> {
    build_guest(
        sh,
        "isola-js-runtime",
        None,
        |wasi_deps_dir, runtime| Ok(js_libraries(wasi_deps_dir, runtime)),
        Path::new("target/js.wasm"),
        2_097_152,
    )
}

fn build_guest(
    sh: &Shell,
    package: &str,
    extra_env: Option<(&str, &str)>,
    libraries: impl FnOnce(&Path, &Path) -> Result<Vec<ComponentLibrary>>,
    output: &Path,
    stack_size: u32,
) -> Result<()> {
    let wasi_deps_dir = env::var("WASI_PYTHON_DEV").unwrap();
    let rustflags = encode_rustflags(&wasm_rustflags(&wasi_deps_dir));

    let build = scrub_host_build_env(cmd!(
        sh,
        "cargo build --locked -Z build-std=std,panic_abort --release --target {TARGET} -p {package}"
    ))
    .env("CARGO_ENCODED_RUSTFLAGS", &rustflags)
    .env(linker_env_key(), wasm_linker());
    let build = match extra_env {
        Some((key, value)) => build.env(key, value),
        None => build,
    };
    build.run()?;

    let runtime = PathBuf::from(format!(
        "target/{TARGET}/release/{}.wasm",
        package.replace('-', "_")
    ));
    assert_shared_pic_module(&runtime)?;
    let libraries = libraries(Path::new(&wasi_deps_dir), &runtime)?;
    write_component_if_changed(libraries, output, stack_size)?;

    Ok(())
}

fn python_libraries(wasi_deps_dir: &Path, runtime: &Path) -> Result<Vec<ComponentLibrary>> {
    let lib_dir = wasi_deps_dir.join("lib");
    let mut libraries = vec![
        ComponentLibrary::new(
            "libisola_python.so",
            runtime,
            false,
            Some("libisola_python_async.so"),
        ),
        ComponentLibrary::new("libc.so", lib_dir.join("libc.so"), false, None),
        ComponentLibrary::new(
            "libwasi-emulated-signal.so",
            lib_dir.join("libwasi-emulated-signal.so"),
            false,
            None,
        ),
        ComponentLibrary::new(
            "libwasi-emulated-getpid.so",
            lib_dir.join("libwasi-emulated-getpid.so"),
            false,
            None,
        ),
        ComponentLibrary::new(
            "libwasi-emulated-process-clocks.so",
            lib_dir.join("libwasi-emulated-process-clocks.so"),
            false,
            None,
        ),
        ComponentLibrary::new("libc++.so", lib_dir.join("libc++.so"), false, None),
        ComponentLibrary::new("libc++abi.so", lib_dir.join("libc++abi.so"), false, None),
        ComponentLibrary::new(
            "libpython3.15.so",
            lib_dir.join("libpython3.15.so"),
            false,
            None,
        ),
    ];

    let site_packages = lib_dir.join("python3.15/site-packages");
    let pattern = site_packages.join("**/*.so");
    let pattern = pattern
        .to_str()
        .context("WASI Python dependency path is not valid UTF-8")?;
    let mut extension_paths = Vec::new();
    for entry in glob::glob(pattern)? {
        extension_paths.push(entry?);
    }
    extension_paths.sort();

    for path in extension_paths {
        let relative = path
            .strip_prefix(wasi_deps_dir)
            .with_context(|| format!("{} is outside the linker input root", path.display()))?;
        let name = format!("/{}", relative.to_string_lossy().replace('\\', "/"));
        libraries.push(ComponentLibrary::new(name, path, true, None));
    }

    Ok(libraries)
}

fn js_libraries(wasi_deps_dir: &Path, runtime: &Path) -> Vec<ComponentLibrary> {
    let lib_dir = wasi_deps_dir.join("lib");
    vec![
        ComponentLibrary::new(
            "libisola_js.so",
            runtime,
            false,
            Some("libisola_js_async.so"),
        ),
        ComponentLibrary::new("libc.so", lib_dir.join("libc.so"), false, None),
        ComponentLibrary::new(
            "libwasi-emulated-signal.so",
            lib_dir.join("libwasi-emulated-signal.so"),
            false,
            None,
        ),
        ComponentLibrary::new(
            "libwasi-emulated-getpid.so",
            lib_dir.join("libwasi-emulated-getpid.so"),
            false,
            None,
        ),
        ComponentLibrary::new(
            "libwasi-emulated-process-clocks.so",
            lib_dir.join("libwasi-emulated-process-clocks.so"),
            false,
            None,
        ),
    ]
}

/// The guest runtime is linked with `-C relocation-model=pic` and
/// `-C link-arg=-shared`; such a module always carries a `dylink.0` custom
/// section. Its absence means host configuration (e.g. a leaked
/// `CARGO_ENCODED_RUSTFLAGS`) overrode our link flags and the runtime is not
/// usable as a shared library — fail the build here rather than at link time.
fn assert_shared_pic_module(path: &Path) -> Result<()> {
    let module = std::fs::read(path)
        .with_context(|| format!("failed to read guest runtime {}", path.display()))?;
    let mut has_dylink = false;
    for payload in Parser::new(0).parse_all(&module) {
        if let Payload::CustomSection(section) = payload? {
            has_dylink = section.name() == "dylink.0";
            if has_dylink {
                break;
            }
        }
    }
    ensure!(
        has_dylink,
        "{} has no dylink.0 section: it was not linked as a shared PIC module \
         (check for host RUSTFLAGS/CARGO_ENCODED_RUSTFLAGS leaking into the guest build)",
        path.display()
    );
    Ok(())
}

fn hash_field(hasher: &mut FingerprintHasher, name: &str, value: &[u8]) {
    hasher.write(&u64::try_from(name.len()).unwrap().to_le_bytes());
    hasher.write(name.as_bytes());
    hasher.write(&u64::try_from(value.len()).unwrap().to_le_bytes());
    hasher.write(value);
}

fn component_hasher(stack_size: u32) -> FingerprintHasher {
    let mut hasher = FingerprintHasher::new();
    hash_field(&mut hasher, "stack-size", &stack_size.to_le_bytes());
    for (name, contents) in COMPONENT_BUILD_INPUTS {
        hash_field(&mut hasher, name, contents);
    }
    hasher
}

fn component_metadata_fingerprint(
    libraries: &[ComponentLibrary],
    stack_size: u32,
) -> Result<String> {
    let mut hasher = component_hasher(stack_size);
    for library in libraries {
        hash_field(&mut hasher, "library-name", library.name.as_bytes());
        hash_field(
            &mut hasher,
            "library-path",
            library.path.to_string_lossy().as_bytes(),
        );
        hash_field(&mut hasher, "library-dlopen", &[u8::from(library.dlopen)]);
        hash_field(
            &mut hasher,
            "library-async-shim",
            library.async_shim_name.unwrap_or_default().as_bytes(),
        );
        let metadata = library.path.metadata().with_context(|| {
            format!(
                "failed to read component library metadata {}",
                library.path.display()
            )
        })?;
        hash_field(&mut hasher, "library-size", &metadata.len().to_le_bytes());
        let (after_epoch, modified) = match metadata.modified()?.duration_since(UNIX_EPOCH) {
            Ok(modified) => (true, modified),
            Err(error) => (false, error.duration()),
        };
        hash_field(
            &mut hasher,
            "library-modified-after-epoch",
            &[u8::from(after_epoch)],
        );
        hash_field(
            &mut hasher,
            "library-modified-seconds",
            &modified.as_secs().to_le_bytes(),
        );
        hash_field(
            &mut hasher,
            "library-modified-nanoseconds",
            &modified.subsec_nanos().to_le_bytes(),
        );
    }
    Ok(hasher.finish())
}

fn component_content_fingerprint(libraries: &[LoadedComponentLibrary], stack_size: u32) -> String {
    let mut hasher = component_hasher(stack_size);
    for library in libraries {
        hash_field(&mut hasher, "library-name", library.name.as_bytes());
        hash_field(&mut hasher, "library-module", &library.module);
        hash_field(&mut hasher, "library-dlopen", &[u8::from(library.dlopen)]);
        hash_field(
            &mut hasher,
            "library-async-shim",
            library.async_shim_name.unwrap_or_default().as_bytes(),
        );
    }
    hasher.finish()
}

fn write_component_fingerprints(
    path: &Path,
    metadata_fingerprint: &str,
    content_fingerprint: &str,
) -> Result<()> {
    std::fs::write(
        path,
        format!("{metadata_fingerprint}\n{content_fingerprint}\n"),
    )
    .with_context(|| format!("failed to write component fingerprint {}", path.display()))
}

fn write_component_if_changed(
    libraries: Vec<ComponentLibrary>,
    output: &Path,
    stack_size: u32,
) -> Result<()> {
    let metadata_fingerprint = component_metadata_fingerprint(&libraries, stack_size)?;
    let fingerprint_path = output.with_extension("wasm.fingerprint");
    let cached = std::fs::read_to_string(&fingerprint_path).unwrap_or_default();
    let mut cached = cached.lines();
    let cached_metadata_fingerprint = cached.next().unwrap_or_default();
    let cached_content_fingerprint = cached.next().unwrap_or_default();
    if output.is_file() && cached_metadata_fingerprint == metadata_fingerprint {
        return Ok(());
    }

    let libraries = libraries
        .into_iter()
        .map(ComponentLibrary::load)
        .collect::<Result<Vec<_>>>()?;
    let content_fingerprint = component_content_fingerprint(&libraries, stack_size);
    if output.is_file() && cached_content_fingerprint == content_fingerprint {
        write_component_fingerprints(
            &fingerprint_path,
            &metadata_fingerprint,
            &content_fingerprint,
        )?;
        return Ok(());
    }

    println!("Linking {}", output.display());
    let mut linker = wit_component::Linker::default();
    linker.encoder().validate(true);
    linker.stack_size(stack_size).use_built_in_libdl(true);
    for library in libraries {
        linker = link_library(
            linker,
            &library.name,
            &library.module,
            library.dlopen,
            library.async_shim_name,
        )?;
    }
    // No preview1 adapter: every linked library is wasip2-native, so there are
    // no `wasi_snapshot_preview1` imports left to satisfy (componentize-py
    // keeps the adapter only because its runtime calls `reset_adapter_state`,
    // which isola gates to p1 builds in `runtime::lifecycle`).
    let component = linker.encode()?;

    std::fs::write(output, component)
        .with_context(|| format!("failed to write component {}", output.display()))?;
    write_component_fingerprints(
        &fingerprint_path,
        &metadata_fingerprint,
        &content_fingerprint,
    )?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn library(
        name: &str,
        module: &[u8],
        dlopen: bool,
        async_shim_name: Option<&'static str>,
    ) -> LoadedComponentLibrary {
        LoadedComponentLibrary {
            name: name.to_string(),
            module: module.to_vec(),
            dlopen,
            async_shim_name,
        }
    }

    #[test]
    fn component_content_fingerprint_tracks_every_link_input() {
        let original = component_content_fingerprint(
            &[library("runtime.so", b"runtime", false, Some("shim.so"))],
            1024,
        );

        assert_ne!(
            original,
            component_content_fingerprint(
                &[library("renamed.so", b"runtime", false, Some("shim.so"))],
                1024
            )
        );
        assert_ne!(
            original,
            component_content_fingerprint(
                &[library("runtime.so", b"changed", false, Some("shim.so"))],
                1024
            )
        );
        assert_ne!(
            original,
            component_content_fingerprint(
                &[library("runtime.so", b"runtime", true, Some("shim.so"))],
                1024
            )
        );
        assert_ne!(
            original,
            component_content_fingerprint(&[library("runtime.so", b"runtime", false, None)], 1024)
        );
        assert_ne!(
            original,
            component_content_fingerprint(
                &[library("runtime.so", b"runtime", false, Some("shim.so"))],
                2048
            )
        );
    }
}
