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
# The commit, from `rill 0.1.0 (<commit>; rustc ...)`.
commit="$(sed -n 's/^[^(]*(\([^;)]*\).*/\1/p' <<<"$version")"
stamp="$(date +%Y-%m-%dT%H-%M-%S)"
short="${commit:0:12}"
[[ "$commit" == *-dirty ]] && short="$short-dirty"
mkdir -p "$results"
report="$results/${stamp}_${short:-unknown}_${slug}.txt"

# What the machine is and what state it is in, for telling a change in
# Rill apart from a change in where it ran. Everything is read where it is
# exposed, and left out where it is not.
lscpu_field() {
    if command -v lscpu >/dev/null; then
        lscpu | sed -n "s/^$1:[[:space:]]*//p" | head -n 1
    fi
}

# The CPU's name: x86 has it in /proc/cpuinfo, ARM often only its vendor.
cpu="$(sed -n 's/^model name[[:space:]]*: //p' /proc/cpuinfo 2>/dev/null | head -n 1 || true)"
if [[ -z "$cpu" ]]; then
    model="$(lscpu_field "Model name")"
    [[ "$model" == "-" ]] && model=""
    cpu="$(echo "$(lscpu_field "Vendor ID") $model" | xargs)"
fi
sockets="$(lscpu_field "Socket(s)")"
per_socket="$(lscpu_field "Core(s) per socket")"
cores=""
[[ "$sockets" =~ ^[0-9]+$ && "$per_socket" =~ ^[0-9]+$ ]] && cores="$((sockets * per_socket)) cores, "
max_mhz="$(lscpu_field "CPU max MHz" | sed 's/\..*//')"
cpu_line="${cpu:-unknown}, ${cores}$(nproc) threads${max_mhz:+, up to $max_mhz MHz}"
caches=""
for c in L1d L1i L2 L3; do
    size="$(lscpu_field "$c cache")"
    [[ -n "$size" ]] && caches+="${caches:+, }$c $size"
done

os="$(sed -n 's/^PRETTY_NAME="\{0,1\}\([^"]*\)"\{0,1\}$/\1/p' /etc/os-release 2>/dev/null || true)"
os_line="${os:-$(uname -s)}, $(uname -sr) ($(uname -m))"

# Where it runs: WSL, a virtual machine, a container, or the machine itself.
if grep -qi microsoft /proc/version 2>/dev/null; then
    environment="WSL2 (Linux on Windows, in a virtual machine)"
elif command -v systemd-detect-virt >/dev/null; then
    virt="$(systemd-detect-virt 2>/dev/null || true)"
    if [[ -z "$virt" || "$virt" == none ]]; then
        environment="bare metal"
    else
        environment="virtual machine ($virt)"
    fi
elif [[ -n "$(lscpu_field "Hypervisor vendor")" ]]; then
    environment="virtual machine ($(lscpu_field "Hypervisor vendor"))"
else
    environment="bare metal, as far as can be told"
fi
if [[ -e /.dockerenv || -e /run/.containerenv ]]; then
    environment+=", in a container"
fi

memory="$(awk '/^MemTotal:/ {t = $2} /^MemAvailable:/ {a = $2}
    END { if (t) printf "%.1f GiB, %.1f GiB available", t / 1048576, a / 1048576 }' /proc/meminfo 2>/dev/null || true)"

# On a laptop, running on battery often means a slower clock.
power=""
for supply in /sys/class/power_supply/*; do
    [[ -r "$supply/type" ]] || continue
    case "$(cat "$supply/type")" in
    Mains)
        if [[ "$(cat "$supply/online" 2>/dev/null || true)" == 1 ]]; then
            power="on AC${power:+, $power}"
        else
            power="on battery${power:+, $power}"
        fi
        ;;
    Battery)
        level="$(cat "$supply/capacity" 2>/dev/null || true)"
        state="$(cat "$supply/status" 2>/dev/null || true)"
        power+="${power:+, }battery ${level:-?}%${state:+ ($state)}"
        ;;
    esac
done

governor="$(cat /sys/devices/system/cpu/cpu0/cpufreq/scaling_governor 2>/dev/null || true)"
load_before="$(cut -d' ' -f1-3 /proc/loadavg 2>/dev/null || true)"

case "$RILL" in
/nix/store/*) build_kind="the flake's release package" ;;
*) build_kind="cargo, release, without the audio device" ;;
esac

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
        line="$(printf '%-12s %8s %10s %15s %9s %8s %6s %6s %10s %10s %10s %9s' \
            "$name" \
            "$(field 's/.*load: *\([0-9.]*%\).*/\1/p' "$best")" \
            "$best_ns" \
            "$(field 's/.*worst \([0-9.]*\) ms of a \([0-9.]*\) ms.*/\1\/\2/p' "$best")" \
            "$(field 's/.*; \([0-9]*\) of \([0-9]*\) over budget.*/\1\/\2/p' "$best")" \
            "$(field 's/.*p99 \([0-9.]*\) ms,.*/\1/p' "$best")" \
            "$(field 's/.*kernel [0-9.]* ms (\([0-9.]*%\)).*/\1/p' "$best")" \
            "$(field 's/.*switches (\([0-9]*\) forced).*/\1/p' "$best")" \
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

header="$(printf '%-12s %8s %10s %15s %9s %8s %6s %6s %10s %10s %10s %9s' \
    bench load ns/sample "worst block ms" "over" "p99 ms" kern% forced \
    instr/smp ops/smp registers "build ms")"
load_after="$(cut -d' ' -f1-3 /proc/loadavg 2>/dev/null || true)"

{
    echo "rill benchmarks: $message"
    echo
    echo "date:        $(date '+%Y-%m-%d %H:%M:%S %z')"
    echo "settings:    ${seconds}s of audio per run unless a benchmark says otherwise, seed 1, best of $runs runs, $threads at a time"
    echo
    echo "Build"
    echo "rill:        $version"
    echo "binary:      $RILL ($build_kind)"
    echo
    echo "System"
    echo "os:          $os_line"
    echo "environment: $environment"
    echo "cpu:         $cpu_line"
    [[ -n "$caches" ]] && echo "caches:      $caches (as the OS reports them)"
    [[ -n "$memory" ]] && echo "memory:      $memory"
    [[ -n "$power" ]] && echo "power:       $power"
    echo "governor:    ${governor:-not exposed}"
    [[ -n "$load_before" ]] && echo "system load: $load_before before, $load_after after (1, 5 and 15 minute averages)"
    echo
    echo "load:        time to render / length of the audio, on one core"
    echo "ns/sample:   nanoseconds per frame of output"
    echo "worst block: the slowest callback / the time it had"
    echo "over:        callbacks over their time / all callbacks"
    echo "p99 ms:      99% of callbacks were faster than this; far below the worst means"
    echo "             the worst was a one-off (often the OS), close to it means it recurs"
    echo "kern%:       share of the rendering thread's CPU time spent in the kernel"
    echo "forced:      times the OS took the CPU away from the rendering thread"
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
