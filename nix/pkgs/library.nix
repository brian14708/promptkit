{
  lib,
  pkgs,
  crane,
}:
let
  craneLib = (crane.mkLib pkgs).overrideToolchain (
    p: p.rust-bin.fromRustupToolchainFile ../../rust-toolchain.toml
  );
  src = lib.fileset.toSource {
    root = ../..;
    fileset = lib.fileset.unions [
      ../../crates/isola/wit
      ../../Cargo.lock
      ../../Cargo.toml
      (craneLib.fileset.commonCargoSources ../../crates/isola)
      (craneLib.fileset.commonCargoSources ../../crates/c-api)
      (craneLib.fileset.commonCargoSources ../../crates/c-api-export)
    ];
  };
in
craneLib.buildPackage {
  pname = "isola-c-api";
  inherit src;
  strictDeps = true;

  CARGO_INCREMENTAL = "0";

  CARGO_PROFILE = "release";
  cargoExtraArgs = "-p isola-c-api-export";
  installPhase = ''
    runHook preInstall

    mkdir -p $out/lib

    cp target/release/libisola.* $out/lib/
    rm $out/lib/libisola.d
    runHook postInstall

    cp -r ${../../crates/c-api/include} $out/include
  '';
}
