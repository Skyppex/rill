{
  src,
  naersk,
  pkgs,
  release ? false,
  # The commit being built, for `rill --version` (there is no `.git` here).
  commit ? "unknown",
}:
naersk.buildPackage {
  name = "rill";
  inherit src;
  nativeBuildInputs = [pkgs.pkg-config pkgs.patchelf];
  buildInputs = [pkgs.alsa-lib];
  propagatedBuildInputs = [pkgs.alsa-lib];
  doCheck = false;
  RILL_COMMIT = commit;

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
