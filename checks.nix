{
  src,
  naersk,
  pkgs,
}: {
  cargo-check = naersk.buildPackage {
    inherit src;
    mode = "check";
    nativeBuildInputs = [pkgs.pkg-config];
    buildInputs = [pkgs.alsa-lib];
  };

  cargo-test = naersk.buildPackage {
    inherit src;
    mode = "test";
    nativeBuildInputs = [pkgs.pkg-config];
    buildInputs = [pkgs.alsa-lib];
  };

  cargo-clippy = naersk.buildPackage {
    inherit src;
    mode = "clippy";
    nativeBuildInputs = [pkgs.pkg-config];
    buildInputs = [pkgs.alsa-lib];
  };

  cargo-fmt =
    pkgs.runCommand "cargo-fmt-check" {
      buildInputs = [pkgs.rustfmt pkgs.cargo];
    } ''
      cp -r ${src} ./source
      chmod -R +w ./source
      cd ./source
      cargo fmt --all -- --check
      touch $out
    '';
}
