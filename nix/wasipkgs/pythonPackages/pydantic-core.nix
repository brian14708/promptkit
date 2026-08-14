{
  crane,
  stdenv,
  pkgs,
  makeRustPlatform,
  maturin,
  pkg-config,
  runCommand,
  rust-bin,
  wasipkgs,
}:
let
  inherit (wasipkgs) wasi-optimize-hook sdk python;
  inherit (python) host;
  pydanticCore = host.pkgs.pydantic-core;
  inherit (pydanticCore) version;
  src = pydanticCore.src + "/pydantic-core";
  rustToolchain = rust-bin.fromRustupToolchainFile ../../../rust-toolchain.toml;
  craneLib = (crane.mkLib pkgs).overrideToolchain (
    p: p.rust-bin.fromRustupToolchainFile ../../../rust-toolchain.toml
  );
  rustPlatform = makeRustPlatform {
    rustc = rustToolchain;
    cargo = rustToolchain;
  };
  packageCargoDeps = rustPlatform.fetchCargoVendor {
    inherit src;
    hash = "sha256-5L317YTV7/Bc/YJLLzc745oJntiYkcZupdeUxiQwcOU=";
  };
  mergedCargoDeps = craneLib.vendorMultipleCargoDeps {
    inherit (craneLib.findCargoFiles src) cargoConfigs;
    cargoLockList = [
      "${src}/Cargo.lock"
      "${rustToolchain.passthru.availableComponents.rust-src}/lib/rustlib/src/rust/library/Cargo.lock"
    ];
  };
in
stdenv.mkDerivation rec {
  pname = "${pydanticCore.pname}-wasi";
  inherit version src;
  cargoDeps = runCommand "pydantic-core-wasi-deps" { } ''
    mkdir -p "$out/.cargo"
    ln -s ${mergedCargoDeps}/config.toml "$out/.cargo/config.toml"
    ln -s ${packageCargoDeps}/Cargo.lock "$out/Cargo.lock"

    for dep in ${mergedCargoDeps}/*; do
      name="$(basename "$dep")"
      case "$name" in
        config.toml) continue ;;
      esac
      ln -s "$dep" "$out/$name"
    done
  '';
  dontStrip = true;

  nativeBuildInputs = [
    python.host
    maturin
    rustToolchain
    rustPlatform.cargoSetupHook
    sdk
    pkg-config
    wasi-optimize-hook
  ];

  buildInputs = [ python ];

  patches = [ ./pydantic-core.patch ];

  postPatch = ''
    substituteInPlace build.rs \
      --replace-fail \
      '    println!("cargo:rustc-env=PROFILE={}", std::env::var("PROFILE").unwrap());' \
      '    println!("cargo:rustc-env=PROFILE={}", std::env::var("PROFILE").unwrap());
    println!("cargo:rustc-link-arg=-shared");
    println!("cargo:rustc-link-arg=-Wl,--allow-undefined");'
  '';

  configurePhase = ''
    runHook preConfigure

    export PYTHONPATH=${python}/lib/python3.15
    export _PYTHON_SYSCONFIGDATA_NAME=_sysconfigdata__wasi_wasm32-wasi
    export _PYTHON_HOST_PLATFORM=wasi-wasm32
    export PYO3_CROSS_LIB_DIR=${python}/lib

    export CARGO_BUILD_TARGET=wasm32-wasip2
    export CARGO_TARGET_WASM32_WASIP2_LINKER=${sdk}/bin/clang
    export CC="${sdk}/bin/clang --sysroot=${sdk}/share/wasi-sysroot"
    export AR="${sdk}/bin/llvm-ar"
    export RANLIB="${sdk}/bin/llvm-ranlib"
    export LDSHARED="${sdk}/bin/clang --sysroot=${sdk}/share/wasi-sysroot"

    export RUSTFLAGS="-Clink-self-contained=no -Crelocation-model=pic -Clink-args=-Wl,--skip-wit-component -Clink-args=-L${python}/lib -Clink-args=-L${sdk}/share/wasi-sysroot/lib/wasm32-wasip2"

    runHook postConfigure
  '';

  buildPhase = ''
    runHook preBuild

    maturin build \
      -Z build-std=std,panic_abort \
      --release \
      --target wasm32-wasip2 \
      -i python3.15 \
      --out dist

    runHook postBuild
  '';

  installPhase = ''
    runHook preInstall

    mkdir -p $out/lib/python3.15/site-packages
    python3 -m zipfile -e \
      dist/pydantic_core-${version}-cp315-cp315-*.whl \
      $out/lib/python3.15/site-packages

    runHook postInstall
  '';
}
