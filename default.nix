{
  src,
  naersk,
  pkgs,
  release ? false,
}:
naersk.buildPackage {
  name = "rill";
  inherit src;
  nativeBuildInputs = [pkgs.pkg-config pkgs.patchelf];
  buildInputs = [pkgs.alsa-lib];
  propagatedBuildInputs = [pkgs.alsa-lib];
  doCheck = false;

  cargoBuildFlags = (
    ["--features=pulseaudio"]
    ++ (
      if release
      then ["--release"]
      else []
    )
  );

  postInstall = ''
    patchelf --set-rpath "${pkgs.alsa-lib}/lib" $out/bin/rill
  '';
}
