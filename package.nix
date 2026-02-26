{
  pkgs,
  buildRustPackage,
  ...
}:
buildRustPackage {
  src = ./.;
  extraArgs = {
    strictDeps = true;
    nativeBuildInputs = [pkgs.pkg-config];
    buildInputs = [pkgs.openssl];
    nativeCheckInputs = [pkgs.git];
  };
}
