#!/bin/bash
# Automated TSan race analysis with HALB_Mutex tracing.
#
# Pass 1: Run tests with ThreadSanitizer (same as run_sanitizers.sh),
#          repeating up to MAX_ATTEMPTS times until races are detected.
# Pass 2: For each flagged test, run HALB_Mutex tracing via trace_mutex.sh
#          to check for shared mutex evidence. This is slow (~1-2h per test
#          due to LLDB breakpoint overhead).
#
# Output files:
#   tsan-warnings.log       Full TSan test output (from the run that caught races)
#   tsan-flagged-tests.txt  One test name per line (tests that triggered warnings)
#   trace-*.log             HALB_Mutex trace per flagged test

set -e

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
MAX_ATTEMPTS=3

toolchain=$(rustup default)
echo "\nUse Rust toolchain: $toolchain"

if [[ $toolchain != nightly* ]]; then
    echo "TSan analysis requires Rust nightly. Skip."
    exit
fi

TARGET=$(rustc -vV | grep host | cut -d' ' -f2)
echo "Target: $TARGET"

# ==========================================================================
# Pass 1: Run tests with TSan, capture warnings
# ==========================================================================
echo "\n\nPass 1: TSan detection\n=============================="

cargo clean

export RUSTFLAGS="-Zsanitizer=thread -Cunsafe-allow-abi-mismatch=sanitizer"
export CARGO_HOST_RUSTFLAGS=""
export SANITIZER_BUILD=1
export TSAN_OPTIONS="halt_on_error=0"

cargo_test_flags="-Z build-std --target ${TARGET}"

# Build once so compilation output doesn't pollute test output.
echo "Building with TSan..."
cargo test --no-run --verbose --lib --tests ${cargo_test_flags} 2>&1 | tail -5

FLAGGED_COUNT=0
for attempt in $(seq 1 ${MAX_ATTEMPTS}); do
    echo "\nAttempt ${attempt}/${MAX_ATTEMPTS}..."

    # Run tests the same way as run_sanitizers.sh -> run_tests.sh.
    set +e
    sh run_tests.sh "${cargo_test_flags}" > tsan-warnings.log 2>&1
    set -e

    # Parse: track current test name, record when TSan warning appears.
    awk '
    /^test .* \.\.\./ { current = $2 }
    /ThreadSanitizer/ { if (current) print current }
    ' tsan-warnings.log | sort -u > tsan-flagged-tests.txt

    FLAGGED_COUNT=$(wc -l < tsan-flagged-tests.txt | tr -d ' ')

    if [[ "${FLAGGED_COUNT}" -gt 0 ]]; then
        echo "TSan flagged ${FLAGGED_COUNT} test(s) on attempt ${attempt}."
        break
    fi
    echo "No TSan warnings on attempt ${attempt}."
done

unset TSAN_OPTIONS
unset RUSTFLAGS
unset CARGO_HOST_RUSTFLAGS
unset SANITIZER_BUILD

if [[ "${FLAGGED_COUNT}" -eq 0 ]]; then
    echo "\nNo TSan warnings detected after ${MAX_ATTEMPTS} attempts. Done."
    exit 0
fi

echo "\nFlagged tests:"
while read t; do echo "  - ${t}"; done < tsan-flagged-tests.txt

# ==========================================================================
# Pass 2: HALB_Mutex tracing on flagged tests
# ==========================================================================
echo "\n\nPass 2: HALB_Mutex tracing\n=============================="

# Clean TSan-instrumented artifacts; trace_mutex.sh does a normal debug build.
cargo clean

while IFS= read -r test_name; do
    echo "\nTracing: ${test_name}"
    "${SCRIPT_DIR}/trace_mutex.sh" "${test_name}" || true
done < tsan-flagged-tests.txt

# ==========================================================================
# Summary: print analysis for each trace log
# ==========================================================================
echo "\n\nAnalysis Summary\n=============================="

for log in trace-*.log; do
    if [[ -f "${log}" ]]; then
        echo "\n--- ${log} ---"
        python3 "${SCRIPT_DIR}/trace_mutex.py" "${log}"
    fi
done

echo "\n\n=== TSan Analysis Complete ==="
echo "TSan output:    tsan-warnings.log"
echo "Flagged tests:  tsan-flagged-tests.txt (${FLAGGED_COUNT})"
echo "Trace logs:     trace-*.log"
