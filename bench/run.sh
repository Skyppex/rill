#!/usr/bin/env bash
# Runs the benchmarks in bench/ and writes a report to bench/results/,
# named after when it ran, the commit rill was built from, and what the run
# is about, so a report can be found again by the change it measured.
#
#   bench/run.sh -m "split run_vector"              every benchmark
#   bench/run.sh -m "noise in lanes" noise song     only these
#   bench/run.sh -m ... --seconds 2                 shorter runs
#   bench/run.sh -m ... --runs 5                    best of 5 instead of 3
#   bench/run.sh -m ... --threads 4                 4 benchmarks at a time
#
# -m (or --message) is required: a few words on what changed, or what the
# run is for. It goes at the end of the report's name and at its top.
#
# Benchmarks running side by side share caches, memory and the CPU's power,
# so they come out slower and noisier than one at a time (the default):
# good for a quick look, not for numbers to compare.
#
# Builds rill in release mode first (without the audio device, which
# benchmarks do not need), unless RILL names a rill binary to use instead.
# `nix run .#bench` sets RILL to the flake's release build.
#
# A benchmark can ask for its own settings with a line of its own:
#
#   // bench: --block 16 --seconds 1
set -euo pipefail

root="${RILL_ROOT:-$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)}"
bench="$root/bench"
results="$bench/results"

seconds=10
runs=3
threads=1
names=()
message=""

usage() {
    sed -n '2,/^set -euo/{/^set -euo/d;s/^# \{0,1\}//;p}' "${BASH_SOURCE[0]}"
}

while (($#)); do
    case "$1" in
    --seconds)
        seconds="$2"
        shift 2
        ;;
    --runs)
        runs="$2"
        shift 2
        ;;
    -m | --message)
        message="$2"
        shift 2
        ;;
    --threads)
        threads="$2"
        shift 2
        ;;
    -h | --help)
        usage
        exit 0
        ;;
    -*)
        echo "bench: unknown option $1" >&2
        exit 2
        ;;
    *)
        names+=("$1")
        shift
        ;;
    esac
done

# Without the spaces around it.
message="${message#"${message%%[![:space:]]*}"}"
message="${message%"${message##*[![:space:]]}"}"
# A few words for the file name: lowercase, with anything but letters and
# digits turned into single dashes.
slug="$(tr '[:upper:]' '[:lower:]' <<<"$message" | sed -E 's/[^a-z0-9]+/-/g; s/^-+//; s/-+$//' | cut -c1-60 | sed -E 's/-+$//')"
if [[ -z "$slug" ]]; then
    echo "bench: say what this run is about, as in: bench/run.sh -m \"split run_vector\"" >&2
    exit 2
fi

for n in "$seconds:--seconds" "$runs:--runs" "$threads:--threads"; do
    value="${n%%:*}"
    if [[ "${n#*:}" == --seconds ]]; then
        [[ "$value" =~ ^[0-9]*\.?[0-9]+$ ]] && continue
    elif [[ "$value" =~ ^[1-9][0-9]*$ ]]; then
        continue
    fi
    echo "bench: ${n#*:} takes a positive number, not $value" >&2
    exit 2
done

if [[ ! -d "$bench" ]]; then
    echo "bench: no benchmarks in $bench; run this from the rill repository" >&2
    exit 1
fi

if [[ -z "${RILL:-}" ]]; then
    echo "bench: building rill (release)" >&2
    cargo build --release --no-default-features \
        --manifest-path "$root/Cargo.toml" \
        --target-dir "$root/target/bench" >&2
    RILL="$root/target/bench/release/rill"
fi

files=()
if ((${#names[@]})); then
    for name in "${names[@]}"; do
        file="$bench/${name%.rill}.rill"
        if [[ ! -f "$file" ]]; then
            echo "bench: no benchmark called $name" >&2
            exit 2
        fi
        files+=("$file")
    done
else
    files=("$bench"/*.rill)
fi

version="$("$RILL" --version)"
# The commit, from `rill 0.1.0 (<commit>)`.
commit="$(sed -n 's/.*(\(.*\))$/\1/p' <<<"$version")"
stamp="$(date +%Y-%m-%dT%H-%M-%S)"
short="${commit:0:12}"
[[ "$commit" == *-dirty ]] && short="$short-dirty"
mkdir -p "$results"
report="$results/${stamp}_${short:-unknown}_${slug}.txt"

# The CPU's name: x86 has it in /proc/cpuinfo, ARM often only its vendor.
cpu="$(sed -n 's/^model name[[:space:]]*: //p' /proc/cpuinfo 2>/dev/null | head -n 1)"
if [[ -z "$cpu" ]] && command -v lscpu >/dev/null; then
    model="$(lscpu | sed -n 's/^Model name:[[:space:]]*//p' | head -n 1)"
    vendor="$(lscpu | sed -n 's/^Vendor ID:[[:space:]]*//p' | head -n 1)"
    [[ "$model" == "-" ]] && model=""
    cpu="$(echo "$vendor $model" | xargs)"
fi
machine="${cpu:+$cpu }$(uname -m), $(nproc) threads, $(uname -sr)"

# One field from `rill profile`'s output.
field() {
    sed -n "$1" <<<"$2" | head -n 1
}

# Each benchmark writes its row and its details here, by its position in
# the list, so the report keeps the list's order however they finish.
work="$(mktemp -d)"
trap 'rm -rf "$work"' EXIT
# Stopping the script stops the benchmarks still running, with every
# process they started.
stop() {
    local pid
    for pid in $(pgrep -P "$1"); do
        stop "$pid"
    done
    kill "$1" 2>/dev/null || true
}
trap 'trap - INT TERM; stop $$; exit 130' INT TERM

# Show which run a benchmark is on: in place when one runs at a time, not at
# all when several do (their lines would get in each other's way).
progress() {
    if ((threads == 1)); then
        printf '\rbench: %-12s run %d of %d' "$1" "$2" "$runs" >&2
    fi
}

# Run one benchmark `runs` times and keep its fastest run.
measure() {
    local index="$1" file="$2"
    local name extra args best="" best_ns="" out ns line
    name="$(basename "$file" .rill)"
    # (Rill files end their lines in CRLF.)
    extra="$(sed -n 's|^// bench: ||p' "$file" | head -n 1 | tr -d '\r')"
    args=(--seed 1)
    [[ "$extra" == *--seconds* ]] || args+=(--seconds "$seconds")
    # shellcheck disable=SC2206 # the settings line is split like a command line
    args+=($extra)

    for ((run = 1; run <= runs; run++)); do
        progress "$name" "$run"
        if ! out="$("$RILL" profile "${args[@]}" "$file" 2>&1)"; then
            best="FAILED"$'\n'"$out"
            break
        fi
        ns="$(field 's/.* \([0-9]*\) ns per sample.*/\1/p' "$out")"
        if [[ -z "$best_ns" ]] || ((ns < best_ns)); then
            best="$out"
            best_ns="$ns"
        fi
    done
    ((threads == 1)) && printf '\r\033[K' >&2

    if [[ "$best" == FAILED* ]]; then
        echo "bench: $name failed:"$'\n'"${best#FAILED$'\n'}" >&2
        line="$(printf '%-12s %s' "$name" "failed")"
    else
        line="$(printf '%-12s %8s %10s %15s %9s %10s %10s %10s %9s' \
            "$name" \
            "$(field 's/.*load: *\([0-9.]*%\).*/\1/p' "$best")" \
            "$best_ns" \
            "$(field 's/.*worst \([0-9.]*\) ms of a \([0-9.]*\) ms.*/\1\/\2/p' "$best")" \
            "$(field 's/.*; \([0-9]*\) of \([0-9]*\) over budget.*/\1\/\2/p' "$best")" \
            "$(field 's/.*program: *\([0-9]*\) instructions.*/\1/p' "$best")" \
            "$(field 's/.*(\([0-9]*\) operations).*/\1/p' "$best")" \
            "$(field 's/.* \([0-9]*\) registers.*/\1/p' "$best")" \
            "$(field 's/.*built in \([0-9.]*\) ms.*/\1/p' "$best")")"
    fi
    echo "$line"
    echo "$line" >"$work/$index.row"
    printf '\n%s\n%s\n' "── $name${extra:+ ($extra)}" "$best" >"$work/$index.details"
}

# Keep up to `threads` benchmarks running, starting the next as one ends.
running=0
for index in "${!files[@]}"; do
    if ((running >= threads)); then
        wait -n || true
        running=$((running - 1))
    fi
    measure "$index" "${files[$index]}" &
    running=$((running + 1))
done
wait

table=""
details=""
for index in "${!files[@]}"; do
    table+="$(cat "$work/$index.row")"$'\n'
    details+="$(cat "$work/$index.details")"$'\n'
done

header="$(printf '%-12s %8s %10s %15s %9s %10s %10s %10s %9s' \
    bench load ns/sample "worst block ms" "over" instr/smp ops/smp registers "build ms")"

{
    echo "rill benchmarks: $message"
    echo
    echo "date:     $(date '+%Y-%m-%d %H:%M:%S %z')"
    echo "rill:     $version"
    echo "binary:   $RILL"
    echo "machine:  $machine"
    echo "settings: ${seconds}s of audio per run unless a benchmark says otherwise, seed 1, best of $runs runs, $threads at a time"
    echo
    echo "load:        time to render / length of the audio, on one core"
    echo "ns/sample:   nanoseconds per frame of output"
    echo "worst block: the slowest callback / the time it had"
    echo "over:        callbacks over their time / all callbacks"
    echo "instr/smp:   instructions per sample, after optimising: what is dispatched"
    echo "ops/smp:     operations per sample: the work those do (a vector block counts"
    echo "             each of its instructions once per lane, a sum once per value)"
    echo
    echo "$header"
    printf '%s' "$table"
    echo
    echo "Details (the best run of each)"
    printf '%s' "$details"
} >"$report"

echo
echo "bench: wrote ${report#"$root"/}"
