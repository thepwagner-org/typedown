{pkgs, ...}:
pkgs.mkShell {
  buildInputs = with pkgs; [
    cargo
    rustfmt
    clippy
    rust-analyzer

    pkg-config
    openssl
    git

    check-jsonschema
    yamlfmt
  ];

  RUST_SRC_PATH = "${pkgs.rustPlatform.rustLibSrc}";
}
