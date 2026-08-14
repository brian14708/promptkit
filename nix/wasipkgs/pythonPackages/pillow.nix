{
  stdenv,
  pkg-config,
  wasipkgs,
}:
let
  inherit (wasipkgs)
    wasi-optimize-hook
    sdk
    zlib
    libjpeg
    freetype
    python
    ;
  host = python.host.withPackages (ps: with ps; [ setuptools ]);
in
stdenv.mkDerivation {
  pname = "${host.pkgs.pillow.pname}-wasi";
  inherit (host.pkgs.pillow) version src;
  dontStrip = true;

  nativeBuildInputs = [
    wasi-optimize-hook
    host
    sdk
    pkg-config
  ];

  buildInputs = [
    python
    zlib
    libjpeg
    freetype
  ];

  patches = [ ./pillow.patch ];

  configurePhase = ''
    runHook preConfigure

    export PYTHONPATH=${python}/lib/python3.15
    export _PYTHON_SYSCONFIGDATA_NAME=_sysconfigdata__wasi_wasm32-wasi
    export _PYTHON_HOST_PLATFORM=wasi-wasm32

    export CC="${sdk}/bin/clang --sysroot=${sdk}/share/wasi-sysroot --target=wasm32-wasip2"
    export AR="${sdk}/bin/llvm-ar"
    export RANLIB="${sdk}/bin/llvm-ranlib"
    export LD="${sdk}/bin/wasm-ld"

    export CFLAGS="-fPIC -I${python}/include/python3.15"
    export LDFLAGS="-L${python}/lib -lpython3.15 -ldl -lz"

    runHook postConfigure
  '';

  installPhase = ''
    runHook preInstall

    ${host}/bin/python3 setup.py install \
      --prefix=$out \
      --single-version-externally-managed \
      --root=/

    find $out/ -type d -name "__pycache__" -exec rm -rf {} +
    runHook postInstall
  '';
}
