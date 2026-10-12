{
  pkgs,
  rill,
}: {
  fix = {
    type = "app";
    program = "${pkgs.writeShellApplication {
      name = "cargo-fix";
      runtimeInputs = [pkgs.cargo pkgs.clippy];
      text = ''
        cargo clippy --fix --allow-dirty --allow-staged --all-targets
      '';
    }}/bin/cargo-fix";
    meta = {
      description = "apply linter suggestions";
    };
  };

  # Runs bench/ against the release build and writes a report to
  # bench/results/ (in the current directory, so the benchmarks and results
  # are the ones being worked on): `nix run .#bench -- -m "what changed"`.
  bench = {
    type = "app";
    program = "${pkgs.writeShellApplication {
      name = "rill-bench";
      runtimeInputs = [pkgs.coreutils pkgs.gnused pkgs.findutils pkgs.util-linux];
      text = ''
        export RILL="${rill}/bin/rill"
        export RILL_ROOT="''${RILL_ROOT:-$PWD}"
        if [[ ! -f "$RILL_ROOT/bench/run.sh" ]]; then
          echo "bench: no bench/run.sh here; run this from the rill repository" >&2
          exit 1
        fi
        exec bash "$RILL_ROOT/bench/run.sh" "$@"
      '';
    }}/bin/rill-bench";
    meta = {
      description = "run the benchmarks in bench/";
    };
  };
}
