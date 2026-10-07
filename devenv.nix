{pkgs, ...}: {
  imports = [
    ./tree-sitter-rill/devenv.nix
  ];

  # https://devenv.sh/packages/
  packages = with pkgs; [
    alejandra
    pkg-config
    alsa-lib # cpal's ALSA backend
  ];

  # https://devenv.sh/languages/
  languages.rust.enable = true;
  languages.nix.enable = true;
}
