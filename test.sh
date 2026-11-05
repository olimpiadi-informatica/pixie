#!/usr/bin/env bash
set -xe

SELFDIR="$(realpath "$(dirname "$0")")"
cd "$SELFDIR"

STORAGE_DIR="${SELFDIR}/storage"

rm -rf prof-out
mkdir -p prof-out

# Pinned rather than a floating `nightly`: an untested newer nightly can (and
# has) introduced codegen regressions that only show up on the exotic
# `-C instrument-coverage` + no_std + SIMD combination pixie-client builds with,
# so bump this deliberately rather than silently drifting.
NIGHTLY=nightly-2026-05-23
if ! rustc +$NIGHTLY -V &>/dev/null; then
  NIGHTLY=nightly
fi

SYSROOT=$(rustc +$NIGHTLY --print sysroot)
LLVM_TOOLS_DIR="$SYSROOT/lib/rustlib/$(rustc +$NIGHTLY -Vv | grep host | awk '{print $2}')/bin"
LLVM_COV="$LLVM_TOOLS_DIR/llvm-cov"
LLVM_PROFDATA="$LLVM_TOOLS_DIR/llvm-profdata"

pushd pixie-shared
RUSTFLAGS='-C instrument-coverage' LLVM_PROFILE_FILE=../prof-out/test-pixie-shared-%m-%p.profraw cargo +$NIGHTLY test --no-fail-fast --all-features
popd

pushd pixie-drivers
RUSTFLAGS='-C instrument-coverage' LLVM_PROFILE_FILE=../prof-out/test-pixie-drivers-%m-%p.profraw cargo +$NIGHTLY test --no-fail-fast
popd

pushd pixie-client
RUSTFLAGS='-C instrument-coverage' LLVM_PROFILE_FILE=../prof-out/test-pixie-client-%m-%p.profraw cargo +$NIGHTLY test --target x86_64-unknown-linux-gnu --no-fail-fast
RUSTFLAGS='-Cinstrument-coverage -Zno-profiler-runtime' cargo +$NIGHTLY build -F coverage
popd

pushd pixie-web
trunk build
popd

pushd pixie-server
RUSTFLAGS='-C instrument-coverage' LLVM_PROFILE_FILE=../prof-out/build-pixie-server-%m-%p.profraw cargo +$NIGHTLY build
popd

mkdir -p "${STORAGE_DIR}/tftpboot" "${STORAGE_DIR}/images" "${STORAGE_DIR}/chunks" "${STORAGE_DIR}/admin"
cp "pixie-client/target/x86_64-unknown-uefi/debug/pixie-client.efi" "${STORAGE_DIR}/tftpboot/"
cp -r pixie-web/dist/* "${STORAGE_DIR}/admin/"

[ -f "${STORAGE_DIR}/config.yaml" ] || cp pixie-server/example.config.yaml "${STORAGE_DIR}/config.yaml"

trap '' SIGTERM

if unshare -Urn true 2>/dev/null; then
  UNSHARE_CMD=(unshare -Urn)
else
  UNSHARE_CMD=(sudo unshare -n)
fi

"${UNSHARE_CMD[@]}" bash <<EOF
    set -xe
    ip link set lo up
    ./run_test.sh "${SELFDIR}/storage" "${BENCH_JSON:-}"
EOF

TEST_OBJECTS=$(
  for file in \
    $(
      (
        RUSTFLAGS="-C instrument-coverage" \
          cargo +$NIGHTLY test --manifest-path pixie-shared/Cargo.toml --no-fail-fast --all-features --no-run --message-format=json
        RUSTFLAGS="-C instrument-coverage" \
          cargo +$NIGHTLY test --manifest-path pixie-drivers/Cargo.toml --no-fail-fast --no-run --message-format=json
        RUSTFLAGS="-C instrument-coverage" \
          cargo +$NIGHTLY test --manifest-path pixie-client/Cargo.toml --target x86_64-unknown-linux-gnu --no-fail-fast --no-run --message-format=json
      ) |
        python3 -c 'import sys, json; [print(f) for line in sys.stdin if (d := json.loads(line)).get("profile", {}).get("test") for f in d.get("filenames", [])]' |
        grep -v dSYM -
    ); do
    printf "%s %s " -object $file
  done
)

DEMANGLER_ARGS=()
if command -v rustfilt &>/dev/null; then
  DEMANGLER_ARGS=(-Xdemangler=rustfilt)
fi

"$LLVM_PROFDATA" merge -sparse prof-out/*.profraw -o prof-out/pixie.profdata
"$LLVM_COV" show \
  -instr-profile=prof-out/pixie.profdata \
  -object pixie-server/target/debug/pixie-server \
  -object pixie-client/target/x86_64-unknown-uefi/debug/pixie-client.efi \
  $TEST_OBJECTS \
  "${DEMANGLER_ARGS[@]}" \
  --ignore-filename-regex='/.cargo' \
  --ignore-filename-regex='/.rustup' \
  --format html \
  -o prof-out/html
