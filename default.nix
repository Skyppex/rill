{
  src,
  naersk,
  pkgConfig,
  alsaLib,
  release ? false,
}:
naersk.buildPackage {
  name = "rill";
  inherit src;
  nativeBuildInputs = [pkgConfig];
  buildInputs = [alsaLib];
  doCheck = false;

  cargoBuildFlags = (
    if release
    then ["--release"]
    else []
  );
}
