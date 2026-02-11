#!/bin/bash
# Trace HALB_Mutex lock/unlock activity around CoreAudio synchronization
# operations using LLDB. Works with any test (ignored or not).
#
# Usage:
#   ./trace_mutex.sh <test_name> [extra_bp_patterns]     Run trace
#   ./trace_mutex.sh --analyze <logfile>                  Re-analyze a saved log
#
#   test_name:         cargo test substring (required)
#   extra_bp_patterns: comma-separated function name patterns for extra
#                      breakpoints, e.g. "my_handler,some_sync_func"

set -e

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
TRACE_MODULE="${SCRIPT_DIR}/trace_mutex.py"

# -- Analyze mode: re-run analysis on a saved log --
if [[ "$1" == "--analyze" ]]; then
    if [[ -z "$2" ]]; then
        echo "Usage: $0 --analyze <logfile>"
        exit 1
    fi
    python3 "${TRACE_MODULE}" "$2"
    exit $?
fi

# -- Trace mode --
if [[ -z "$1" ]]; then
    echo "Usage: $0 <test_name> [extra_bp_patterns]"
    echo "       $0 --analyze <logfile>"
    echo ""
    echo "Examples:"
    echo "  $0 test_vpio_stop_sync_raw"
    echo "  $0 test_ops_duplex_voice_stream_set_input_processing_params"
    echo "  $0 test_audio_object_remove_property_listener_sync"
    echo "  $0 test_vpio_stop_sync_raw my_handler,my_sync_func"
    echo "  $0 --analyze trace-test_vpio_stop_sync_raw-20250209-143021.log"
    exit 1
fi

TEST_NAME="$1"
EXTRA_PATTERNS="${2:-}"
TIMESTAMP=$(date +%Y%m%d-%H%M%S)
LOG_FILE="trace-${TEST_NAME}-${TIMESTAMP}.log"

echo "=== HALB_Mutex Trace ==="
echo "Test: ${TEST_NAME}"
echo "Log:  ${LOG_FILE}"
echo ""

# Build the test binary
echo "Building test binary..."
cargo test -p cubeb-coreaudio --no-run 2>&1 | tail -3

# Find the test binary path from cargo's output
TEST_BIN=$(cargo test -p cubeb-coreaudio --no-run 2>&1 \
    | grep "Executable unittests src/lib.rs" \
    | sed 's/.*(\(.*\))/\1/' \
    | head -1)

if [[ -z "${TEST_BIN}" ]]; then
    echo "ERROR: Could not find test binary."
    echo "Try: cargo test -p cubeb-coreaudio --no-run 2>&1 | grep Executable"
    exit 1
fi

# Resolve relative path
if [[ "${TEST_BIN}" != /* ]]; then
    TEST_BIN="${SCRIPT_DIR}/${TEST_BIN}"
fi

echo "Binary: ${TEST_BIN}"

# Auto-detect whether the test needs --ignored
IGNORED_FLAG=""
if "${TEST_BIN}" --list --ignored 2>/dev/null | grep -q "${TEST_NAME}"; then
    IGNORED_FLAG="--ignored"
    echo "Test type: ignored (behavioral)"
elif "${TEST_BIN}" --list 2>/dev/null | grep -q "${TEST_NAME}"; then
    echo "Test type: normal"
else
    echo "ERROR: Test '${TEST_NAME}' not found in binary."
    echo ""
    echo "Available tests matching pattern:"
    "${TEST_BIN}" --list 2>/dev/null | grep "${TEST_NAME}" || true
    "${TEST_BIN}" --list --ignored 2>/dev/null | grep "${TEST_NAME}" || true
    echo ""
    echo "Behavioral (ignored) tests:"
    "${TEST_BIN}" --list --ignored 2>/dev/null | grep "behaviors::" || true
    exit 1
fi

echo ""
echo "Running under LLDB..."
echo "---"

export TRACE_EXTRA_PATTERNS="${EXTRA_PATTERNS}"

lldb --batch \
    -o "command script import ${TRACE_MODULE}" \
    -o "process launch -- ${TEST_NAME} ${IGNORED_FLAG} --nocapture --test-threads=1" \
    -o "trace_summary" \
    -- "${TEST_BIN}" > "${LOG_FILE}" 2>&1

echo ""
echo "=== Trace Complete ==="
echo "Log saved: ${LOG_FILE}"
echo "Analyze:   $0 --analyze ${LOG_FILE}"
