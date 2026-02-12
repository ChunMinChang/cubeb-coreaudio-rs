#!/bin/bash
# Trace HALB_Mutex contention using dtrace.
# Measures actual Lock() duration — long durations prove the thread was blocked.
#
# Requires: sudo (dtrace needs root on macOS) AND SIP disabled for dtrace.
# The pid provider cannot probe Apple framework symbols (CoreAudio, HALB_Mutex)
# with SIP enabled. To disable SIP for dtrace:
#   1. Boot into Recovery Mode (hold Power on Apple Silicon)
#   2. csrutil enable --without dtrace
#   3. Reboot
# Check current status: csrutil status
# For SIP-enabled machines, use trace_mutex.sh (LLDB-based) instead.
#
# Usage:
#   sudo ./trace_contention.sh <test_name>
#   sudo ./trace_contention.sh --analyze <logfile>
#
# Examples:
#   sudo ./trace_contention.sh test_ops_duplex_voice_stream_set_input_processing_params
#   sudo ./trace_contention.sh test_vpio_stop_sync_raw

set -e

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
DTRACE_SCRIPT="${SCRIPT_DIR}/trace_contention.d"

# -- Analyze mode --
if [[ "$1" == "--analyze" ]]; then
    if [[ -z "$2" ]]; then
        echo "Usage: $0 --analyze <logfile>"
        exit 1
    fi
    logfile="$2"
    echo "=== Contention Analysis: ${logfile} ==="
    echo ""

    echo "Sync operations:"
    grep "RETURN" "${logfile}" | grep -v "dtrace" | while read line; do
        echo "  ${line}"
    done

    echo ""
    echo "Contended locks (>100us):"
    grep "CONTENTION" "${logfile}" | while read line; do
        echo "  ${line}"
    done

    contention_count=$(grep -c "CONTENTION" "${logfile}" 2>/dev/null || echo 0)
    total_locks=$(grep -c "HALB_Mutex::Lock" "${logfile}" 2>/dev/null || echo 0)
    echo ""
    echo "Total Lock events: ${total_locks}"
    echo "Contended (>100us): ${contention_count}"
    if [[ "${total_locks}" -gt 0 ]]; then
        pct=$((contention_count * 100 / total_locks))
        echo "Contention rate: ${pct}%"
    fi
    exit 0
fi

# -- Trace mode --
if [[ -z "$1" ]]; then
    echo "Usage: sudo $0 <test_name>"
    echo "       $0 --analyze <logfile>"
    exit 1
fi

if [[ $EUID -ne 0 ]]; then
    echo "ERROR: dtrace requires root. Run with sudo."
    exit 1
fi

TEST_NAME="$1"
TIMESTAMP=$(date +%Y%m%d-%H%M%S)
LOG_NAME=$(echo "${TEST_NAME}" | sed 's/::/-/g')
LOG_FILE="dtrace-${LOG_NAME}-${TIMESTAMP}.log"

echo "=== HALB_Mutex Contention Trace (dtrace) ==="
echo "Test: ${TEST_NAME}"
echo "Log:  ${LOG_FILE}"
echo ""

# Build the test binary
echo "Building test binary..."
cargo test -p cubeb-coreaudio --no-run 2>&1 | tail -3

TEST_BIN=$(cargo test -p cubeb-coreaudio --no-run 2>&1 \
    | grep "Executable unittests src/lib.rs" \
    | sed 's/.*(\(.*\))/\1/' \
    | head -1)

if [[ -z "${TEST_BIN}" ]]; then
    echo "ERROR: Could not find test binary."
    exit 1
fi

if [[ "${TEST_BIN}" != /* ]]; then
    TEST_BIN="${SCRIPT_DIR}/${TEST_BIN}"
fi

echo "Binary: ${TEST_BIN}"

# Auto-detect --ignored
IGNORED_FLAG=""
if "${TEST_BIN}" --list --ignored 2>/dev/null | grep -q "${TEST_NAME}"; then
    IGNORED_FLAG="--ignored"
    echo "Test type: ignored (behavioral)"
elif "${TEST_BIN}" --list 2>/dev/null | grep -q "${TEST_NAME}"; then
    echo "Test type: normal"
else
    echo "ERROR: Test '${TEST_NAME}' not found."
    exit 1
fi

echo ""
echo "Running dtrace (low overhead, real timing)..."
echo "---"

dtrace -s "${DTRACE_SCRIPT}" \
    -c "${TEST_BIN} ${TEST_NAME} ${IGNORED_FLAG} --nocapture --test-threads=1" \
    > "${LOG_FILE}" 2>&1

echo ""
echo "=== Trace Complete ==="
echo "Log saved: ${LOG_FILE}"
echo "Analyze:   $0 --analyze ${LOG_FILE}"

# Auto-analyze
echo ""
"$0" --analyze "${LOG_FILE}"
