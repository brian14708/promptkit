{ pkgs, ... }:
{
  projectRootFile = "flake.nix";
  programs = {
    nixfmt.enable = true;
    rustfmt = {
      enable = true;
      package = pkgs.rust-bin.fromRustupToolchainFile ../rust-toolchain.toml;
    };
    ruff-format.enable = true;
    clang-format.enable = true;
    actionlint.enable = true;
  };
  settings.formatter = {
    clang-format = {
      excludes = [
        "crates/c-api/include/isola.h"
      ];
    };
  };
}
