#!/bin/bash
# Runs the smoke cases under the freshly built host: compiles each with the
# compiler named by $GHUL_COMPILER (a ghul.dll run by dotnet), runs the module
# under $GHUL_WASMTIME_HOST, and byte-checks the output against run.expected.
#
# usage: tests/smoke.sh
#
# Environment:
#   GHUL_COMPILER        the compiler dll to run with dotnet (required)
#   GHUL_WASMTIME_HOST   the host binary to test (default target/release/ghul-wasmtime)
#   GHUL_CORE            a ghul-core checkout (default libraries/ghul-core)
#   GHUL_RUNTIME         a ghul-runtime checkout (default libraries/ghul-runtime)

set -euo pipefail

root=$(cd "$(dirname "$0")/.." && pwd)

compiler=${GHUL_COMPILER:?GHUL_COMPILER names the compiler dll to test against}
host=${GHUL_WASMTIME_HOST:-$root/target/release/ghul-wasmtime}
core=${GHUL_CORE:-$root/libraries/ghul-core}
runtime=${GHUL_RUNTIME:-$root/libraries/ghul-runtime}

work=${GHUL_SMOKE_WORK:-$(mktemp -d)}
mkdir -p "$work"
trap 'rm -rf "$work"' EXIT

{
    echo --library-source
    echo "ghul-core@$(tr -d '[:space:]' < "$core/VERSION")=$core"
    find "$core/src" -name '*.ghul' | sort

    echo --library-source
    echo "ghul-runtime@$(tr -d '[:space:]' < "$runtime/VERSION")=$runtime"
    while read -r file; do
        [ -n "$file" ] && echo "$runtime/$file"
    done < "$runtime/wasm-sources.rsp"
} > "$work/libraries.rsp"

status=0

for case_dir in "$root"/tests/cases/*/; do
    name=$(basename "$case_dir")
    case_work="$work/$name"
    mkdir -p "$case_work"

    if ! timeout 120 dotnet "$compiler" --target wasm -o "$case_work/program" \
            @"$work/libraries.rsp" "$case_dir/prog.ghul" > "$case_work/compile.log" 2>&1; then
        echo "$name: compile failed"
        head -5 "$case_work/compile.log"
        status=1
        continue
    fi

    host_status=0
    (cd "$case_work" && timeout 30 "$host" "$case_work/program.wasm" \
        > "$case_work/out" 2> "$case_work/host.err") || host_status=$?

    if [ "$host_status" -ne 0 ]; then
        echo "$name: host failed (exit $host_status)"
        head -10 "$case_work/host.err"
        status=1
        continue
    fi

    if cmp -s "$case_work/out" "$case_dir/run.expected"; then
        echo "$name: pass"
    else
        echo "$name: output differs"
        diff "$case_dir/run.expected" "$case_work/out" | head -5
        status=1
    fi
done

exit $status
