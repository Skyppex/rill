{
  src,
  naersk,
  pkgConfig,
  release ? false,
}:
naersk.buildPackage {
  name = "rill";
  inherit src;
  nativeBuildInputs = [pkgConfig];
  doCheck = false;

  cargoBuildFlags = (
    if release
    then ["--release"]
    else []
  );
}
