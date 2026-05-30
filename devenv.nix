{ pkgs, ... }:

{
  env = {
    GREET = "devenv";
    RUSTC_WRAPPER = "sccache";
  };

  packages = with pkgs; [
    git
    cmake
    sccache
    prek
  ];

  languages.rust = {
    enable = true;
    channel = "stable";
    components = [
      "rustc"
      "cargo"
      "clippy"
      "rustfmt"
      "rust-analyzer"
      "rust-src"
    ];
  };

}
