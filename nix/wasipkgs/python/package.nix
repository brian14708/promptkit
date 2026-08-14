{
  stdenv,
  python315,
  pkg-config,
  wasmtime,
  wasipkgs,
}:
let
  inherit (wasipkgs)
    wasi-optimize-hook
    zlib
    zstd
    sdk
    ;
  host = python315.override {
    packageOverrides = _final: prev: {
      # Pydantic's Nix test-only closure is not yet Python 3.15 compatible.
      # Runtime dependency and import checks remain enabled.
      pydantic = prev.pydantic.overridePythonAttrs (_old: {
        doCheck = false;
      });
    };
  };
in
stdenv.mkDerivation rec {
  pname = "${host.pname}-wasi";
  inherit (host) version src;
  dontStrip = true;

  buildInputs = [
    zlib
    zstd
  ];

  passthru = {
    inherit host;
  };

  nativeBuildInputs = [
    pkg-config
    host
    wasmtime
    wasi-optimize-hook
  ];

  buildArch =
    if stdenv.hostPlatform.isDarwin then
      if stdenv.hostPlatform.isAarch64 then "arm64-apple-darwin" else "x86_64-apple-darwin"
    else if stdenv.hostPlatform.isLinux then
      if stdenv.hostPlatform.isAarch64 then "aarch64-unknown-linux-gnu" else "x86_64-unknown-linux-gnu"
    else
      builtins.throw "Unsupported platform for Python WASI build";

  configureFlags = [
    "--prefix=/"
    "--host=wasm32-wasip1"
    "--build=${buildArch}"
    "--with-build-python=${host}/bin/python3"
    "--enable-shared"
    "--disable-test-modules"
    "--without-system-libmpdec"
    "--with-tzpath=/lib/python3.15/site-packages/tzdata/zoneinfo"
    "--enable-big-digits=30"
    "--with-pymalloc"
  ];

  preConfigure = ''
    export CONFIG_SITE=$PWD/Platforms/WASI/config.site-wasm32-wasi
    export HOSTRUNNER="${wasmtime}/bin/wasmtime run --argv0 /python.wasm --dir $PWD::/ --config $PWD/Platforms/WASI/wasmtime.toml"
    # Keep --host=wasm32-wasi (so config.site and the sysconfigdata names stay
    # put) and select preview2 purely through the compiler target, the same
    # split componentize-py uses.
    export CFLAGS="$CFLAGS --target=wasm32-wasip2 -fPIC"
    export LDFLAGS="$LDFLAGS --target=wasm32-wasip2"
    export CC="${sdk}/bin/clang --sysroot=${sdk}/share/wasi-sysroot --target=wasm32-wasip2"
    export CXX="${sdk}/bin/clang++ --sysroot=${sdk}/share/wasi-sysroot --target=wasm32-wasip2"
    export AR=${sdk}/bin/llvm-ar
    export RANLIB=${sdk}/bin/ranlib
  '';

  buildPhase = ''
    runHook preBuild
    make build_all
    runHook postBuild
  '';

  installPhase = ''
    runHook preInstall
    DESTDIR=$out make install
    runHook postInstall
  '';

  postInstall = ''
    export _PYTHON_HOST_PLATFORM=wasi-wasm32
    export _PYTHON_SYSCONFIGDATA_NAME=_sysconfigdata__wasi_wasm32-wasi

    wasm_assets_script="$PWD/Platforms/emscripten/wasm_assets.py"
    if [ ! -e "$wasm_assets_script" ]; then
      wasm_assets_script="$PWD/Tools/wasm/emscripten/wasm_assets.py"
    fi

    ${host}/bin/python3 "$wasm_assets_script" \
      --prefix $out \
      --output $out/lib/python315.zip

    clang_rt_builtins="$(
      find ${sdk}/lib/clang -path '*/lib/wasm32-unknown-wasip2/libclang_rt.builtins.a' -print -quit
    )"
    test -n "$clang_rt_builtins"

    touch $out/python-stub.c
    $CC $CFLAGS \
      -shared -o $out/lib/libpython3.15.so \
      $out/python-stub.c \
      -Wl,--whole-archive $out/lib/libpython3.15.a -Wl,--no-whole-archive \
      $(pkg-config --libs zlib) \
      $(pkg-config --libs libzstd) \
      $PWD/Modules/_hacl/libHacl_Hash_SHA1.a \
      $PWD/Modules/_hacl/libHacl_Hash_SHA2.a \
      $PWD/Modules/_hacl/libHacl_Hash_SHA3.a \
      $PWD/Modules/_hacl/libHacl_Hash_MD5.a \
      $PWD/Modules/_hacl/libHacl_Hash_BLAKE2.a \
      $PWD/Modules/_hacl/libHacl_HMAC.a \
      $PWD/Modules/_decimal/libmpdec/libmpdec.a \
      $PWD/Modules/expat/libexpat.a \
      "$clang_rt_builtins" \
      ${sdk}/share/wasi-sysroot/lib/wasm32-wasip2/libwasi-emulated-signal.so \
      ${sdk}/share/wasi-sysroot/lib/wasm32-wasip2/libwasi-emulated-process-clocks.so \
      ${sdk}/share/wasi-sysroot/lib/wasm32-wasip2/libwasi-emulated-getpid.so \
      ${sdk}/share/wasi-sysroot/lib/wasm32-wasip2/libdl.so \
      ${sdk}/share/wasi-sysroot/lib/wasm32-wasip2/libc.so
    rm $out/lib/libpython3.15.a $out/python-stub.c

    # CPython's WASI build-details.json is missing ABI suffix metadata that
    # maturin expects for cross-compilation.
    ${host}/bin/python3 - <<'PY'
    import json
    from pathlib import Path

    root = Path("${placeholder "out"}/lib/python3.15")
    build_details_path = root / "build-details.json"
    sysconfig_vars_path = root / "_sysconfig_vars__wasi_wasm32-wasi.json"

    with sysconfig_vars_path.open() as f:
        sysconfig_vars = json.load(f)
    with build_details_path.open() as f:
        build_details = json.load(f)

    build_details.setdefault("abi", {})["extension_suffix"] = sysconfig_vars["EXT_SUFFIX"]
    build_details["abi"].setdefault("stable_abi_suffix", ".abi3.so")
    build_details.setdefault("libpython", {})["dynamic"] = "//lib/libpython3.15.so"
    build_details["libpython"].setdefault("link_extensions", False)

    with build_details_path.open("w") as f:
        json.dump(build_details, f, indent=2)
        f.write("\n")
    PY

    rm -rf $out/bin/
    rm -rf $out/lib/python3.15/config-3.15-wasm32-wasi/
    find $out/ -type f -name "*.a" -exec rm {} +
    find $out/ -type d -name "__pycache__" -exec rm -rf {} +
  '';
}
