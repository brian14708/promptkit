{
  lib,
  pkgs,
  crane,
  stdenvNoCC,
  wasipkgs,
  nukeReferences,
  callPackage,
}:
let
  bundle = callPackage ./bundle.nix { };
  inherit (wasipkgs.python.host) pythonVersion;
  pythonSitePackages = "lib/python${pythonVersion}/site-packages";
  extensionPackages = with wasipkgs.pythonPackages; [
    numpy
    pillow
    pydantic-core
  ];
  linkerInputs = stdenvNoCC.mkDerivation {
    pname = "wasi-python-linker-inputs";
    inherit (wasipkgs.python) version;
    dontUnpack = true;

    installPhase = ''
      runHook preInstall

      mkdir -p "$out/lib" "$out/${pythonSitePackages}"
      cp --no-preserve=mode -L ${wasipkgs.python}/lib/libpython*.so "$out/lib/"
      cp --no-preserve=mode -L ${wasipkgs.sdk}/share/wasi-sysroot/lib/wasm32-wasip2/*.so "$out/lib/"
      cp --no-preserve=mode -L ${wasipkgs.sdk}/share/wasi-sysroot/lib/wasm32-wasip2/noeh/*.so "$out/lib/"

      for package in ${builtins.concatStringsSep " " (builtins.map toString extensionPackages)}; do
        (
          cd "$package/${pythonSitePackages}"
          find . -type f -name '*.so' \
            -exec cp --no-preserve=mode -L --parents '{}' "$out/${pythonSitePackages}/" \;
        )
      done

      runHook postInstall
    '';
  };
  rustToolchainFor = p: p.rust-bin.fromRustupToolchainFile ../../../rust-toolchain.toml;
  rustToolchain = rustToolchainFor pkgs;
  craneLib = (crane.mkLib pkgs).overrideToolchain rustToolchainFor;
  src = lib.fileset.toSource {
    root = ../../..;
    fileset = lib.fileset.unions [
      ../../../Cargo.lock
      ../../../Cargo.toml
      ../../../crates/isola/wit
      (craneLib.fileset.commonCargoSources ../../../crates/runtime)
      (craneLib.fileset.commonCargoSources ../../../crates/xtask)
      (craneLib.fileset.commonCargoSources ../../../crates/python-runtime)
    ];
  };
  commonArgs = {
    pname = "isola-python";
    inherit src;
    preConfigure = ''
          # The filtered source includes crates/isola/wit for component metadata, but
          # not the crate manifest/source. Create a minimal stub crate so workspace
          # member discovery succeeds when running xtask.
          if [ ! -f crates/isola/Cargo.toml ]; then
            mkdir -p crates/isola/src
            cat > crates/isola/Cargo.toml <<'EOF'
      [package]
      name = "isola"
      version.workspace = true
      edition.workspace = true
      publish = false

      [lib]
      path = "src/lib.rs"
      EOF
            cat > crates/isola/src/lib.rs <<'EOF'
      // Stub workspace member used by the python Nix build source filter.
      EOF
          fi
    '';
    nativeBuildInputs = [
      wasipkgs.python.host
      nukeReferences
    ];

    env = {
      # Release artifacts are immutable in Nix; incremental state only adds
      # work and prevents reuse of the compiler's normal release cache layout.
      CARGO_INCREMENTAL = "0";
      PYO3_PYTHON = "${wasipkgs.python.host}/bin/python3";
      WASI_PYTHON_DEV = linkerInputs;
      WASI_SDK = wasipkgs.sdk;
    };

    doCheck = false;
    cargoVendorDir = craneLib.vendorMultipleCargoDeps {
      inherit (craneLib.findCargoFiles src) cargoConfigs;
      cargoLockList = [
        ../../../Cargo.lock
        "${rustToolchain.passthru.availableComponents.rust-src}/lib/rustlib/src/rust/library/Cargo.lock"
      ];
    };
  };
  cargoArtifacts = craneLib.buildDepsOnly (
    commonArgs
    // {
      buildPhaseCargoCommand = ''
        cargo build --offline -p xtask
        PYO3_CROSS_PYTHON_VERSION=3.15 \
          CARGO_TARGET_WASM32_WASIP2_LINKER="${wasipkgs.sdk}/bin/clang" \
          RUSTFLAGS="--cfg pyo3_disable_reference_pool -C relocation-model=pic -C link-args=-Wl,--skip-wit-component -C link-arg=-shared -C link-args=-Wl,--allow-undefined -C link-self-contained=n -Lnative=${linkerInputs}/lib" \
          cargo build --offline -Z build-std=std,panic_abort --release \
            --target wasm32-wasip2 -p isola-python-runtime
      '';
    }
  );
in
craneLib.buildPackage (
  commonArgs
  // {
    inherit cargoArtifacts;
    buildPhaseCargoCommand = "cargo run --offline -p xtask -- build-python";
    doNotPostBuildInstallCargoBinaries = true;
    doNotRemoveReferencesToRustToolchain = true;
    doNotRemoveReferencesToVendorDir = true;

    installPhase = ''
      runHook preInstall

      install -Dm644 target/python.wasm $out/bin/python.wasm
      cp --no-preserve=mode -rL ${bundle}/. $out/
      find $out/ -type f -exec nuke-refs '{}' +

      runHook postInstall
    '';

    passthru = {
      inherit bundle cargoArtifacts linkerInputs;
    };
  }
)
