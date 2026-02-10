# The option `Z` is only accepted on the nightly compiler
# so changing to nightly toolchain by `rustup default nightly` is required.

# See: https://github.com/rust-lang/rust/issues/39699 for more sanitizer support.

toolchain=$(rustup default)
echo "\nUse Rust toolchain: $toolchain"

if [[ $toolchain != nightly* ]]; then
    echo "The sanitizer is only available on Rust Nightly only. Skip."
    exit
fi

# Bail out once getting an error.
set -e

# Determine the target triple for the current platform.
# This is needed for -Zbuild-std which requires an explicit target.
TARGET=$(rustc -vV | grep host | cut -d' ' -f2)
echo "Target: $TARGET"

# Ideally, sanitizers should be ("address" "leak" "memory" "thread") but
# - `memory`: It doesn't works with target x86_64-apple-darwin
# - `leak`: Get some errors that are out of our control. See:
#   https://github.com/mozilla/cubeb-coreaudio-rs/issues/45#issuecomment-591642931
sanitizers=("address" "thread")
for san in "${sanitizers[@]}"
do
    San="$(tr '[:lower:]' '[:upper:]' <<< ${san:0:1})${san:1}"
    echo "\n\nRun ${San}Sanitizer\n------------------------------"
    # Clean build artifacts between sanitizer runs to avoid ABI mismatch.
    # Different sanitizers modify the ABI differently, so crates compiled for one
    # sanitizer cannot be reused with another.
    cargo clean
    # Use -Zbuild-std to rebuild std with sanitizer support.
    # RUSTFLAGS applies sanitizer to target code and -Zbuild-std's std rebuild.
    # CARGO_HOST_RUSTFLAGS="" prevents sanitizer from applying to build scripts
    # and proc-macros (they run on host and link against host's non-sanitized std).
    export RUSTFLAGS="-Zsanitizer=${san}"
    # ThreadSanitizer needs -Cunsafe-allow-abi-mismatch=sanitizer to work around
    # a Rust bug where ABI check incorrectly fires even with -Zbuild-std.
    # See: https://github.com/rust-lang/rust/issues/146465
    if [[ "${san}" == "thread" ]]; then
        export RUSTFLAGS="${RUSTFLAGS} -Cunsafe-allow-abi-mismatch=sanitizer"
        # Apply TSan annotations to teach TSan about CoreAudio's internal
        # AudioOutputUnitStop synchronization that it cannot observe.
        if [[ -f tsan-annotations.patch ]]; then
            echo "Applying tsan-annotations.patch..."
            git apply tsan-annotations.patch
            tsan_patch_applied=1
        fi
    fi
    export CARGO_HOST_RUSTFLAGS=""
    # Set SANITIZER_BUILD so run_tests.sh can detect sanitizer mode.
    export SANITIZER_BUILD=1
    cargo_test_flags="-Z build-std --target ${TARGET}"
    if [[ "${tsan_patch_applied:-0}" == "1" ]]; then
        cargo_test_flags="${cargo_test_flags} --features tsan-annotations"
    fi
    sh run_tests.sh "${cargo_test_flags}"
    # Revert TSan annotations patch to keep the tree clean.
    if [[ "${tsan_patch_applied:-0}" == "1" ]]; then
        echo "Reverting tsan-annotations.patch..."
        git apply -R tsan-annotations.patch
        tsan_patch_applied=0
    fi
    unset RUSTFLAGS
    unset CARGO_HOST_RUSTFLAGS
    unset SANITIZER_BUILD
done
