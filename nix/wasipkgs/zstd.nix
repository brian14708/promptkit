{
  stdenv,
  cmake,
  wasipkgs,
  pkg-config,
  zstd,
}:
let
  inherit (wasipkgs) sdk;
in
stdenv.mkDerivation {
  pname = "${zstd.pname}-wasi";
  inherit (zstd) version src;
  dontStrip = true;

  # zstd keeps its CMake project under build/cmake rather than the source root.
  cmakeDir = "../build/cmake";

  nativeBuildInputs = [
    cmake
    pkg-config
  ];

  cmakeFlags = [
    "-DCMAKE_TOOLCHAIN_FILE=${sdk.cmakeToolchain}"
    "-DCMAKE_POSITION_INDEPENDENT_CODE=ON"
    "-DZSTD_BUILD_STATIC=ON"
    "-DZSTD_BUILD_SHARED=OFF"
    # The CLI and the contrib/test helpers assume a hosted platform.
    "-DZSTD_BUILD_PROGRAMS=OFF"
    "-DZSTD_BUILD_CONTRIB=OFF"
    "-DZSTD_BUILD_TESTS=OFF"
    "-DZSTD_LEGACY_SUPPORT=OFF"
    # WASI has no pthreads.
    "-DZSTD_MULTITHREAD_SUPPORT=OFF"
  ];
}
