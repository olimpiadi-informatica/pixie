#!/usr/bin/env bash
set -xe

SELFDIR="$(realpath "$(dirname "$0")")"
cd "$SELFDIR"

STORAGE_DIR="${SELFDIR}/storage"

rm -rf prof-out
mkdir -p prof-out

SYSROOT=$(rustc +nightly --print sysroot)
LLVM_TOOLS_DIR="$SYSROOT/lib/rustlib/$(rustc +nightly -Vv | grep host | awk '{print $2}')/bin"
LLVM_COV="$LLVM_TOOLS_DIR/llvm-cov"
LLVM_PROFDATA="$LLVM_TOOLS_DIR/llvm-profdata"

pushd pixie-shared
RUSTFLAGS='-C instrument-coverage' LLVM_PROFILE_FILE=../prof-out/test-pixie-shared-%m-%p.profraw cargo +nightly test --no-fail-fast --all-features
popd

pushd pixie-uefi
RUSTFLAGS='-Cinstrument-coverage -Zno-profiler-runtime' cargo +nightly build -F coverage
popd

pushd pixie-web
trunk build
popd

pushd pixie-server
RUSTFLAGS='-C instrument-coverage' LLVM_PROFILE_FILE=../prof-out/build-pixie-server-%m-%p.profraw cargo +nightly build
popd

mkdir -p "${STORAGE_DIR}/tftpboot" "${STORAGE_DIR}/images" "${STORAGE_DIR}/chunks" "${STORAGE_DIR}/admin"
cp "pixie-uefi/target/x86_64-unknown-uefi/debug/pixie-uefi.efi" "${STORAGE_DIR}/tftpboot/"
cp -r pixie-web/dist/* "${STORAGE_DIR}/admin/"

[ -f "${STORAGE_DIR}/config.yaml" ] || cp pixie-server/example.config.yaml "${STORAGE_DIR}/config.yaml"

trap '' SIGTERM
sudo ./run_test.sh ${SELFDIR}/storage

TEST_OBJECTS=$(
  for file in \
    $(
      RUSTFLAGS="-C instrument-coverage" \
        cargo +nightly test --manifest-path pixie-shared/Cargo.toml --no-fail-fast --all-features --no-run --message-format=json |
        jq -r "select(.profile.test == true) | .filenames[]" |
        grep -v dSYM -
    ); do
    printf "%s %s " -object $file
  done
)

"$LLVM_PROFDATA" merge -sparse prof-out/*.profraw -o prof-out/pixie.profdata
"$LLVM_COV" show \
  -instr-profile=prof-out/pixie.profdata \
  -object pixie-server/target/debug/pixie-server \
  -object pixie-uefi/target/x86_64-unknown-uefi/debug/pixie-uefi.efi \
  $TEST_OBJECTS \
  -Xdemangler=rustfilt \
  --ignore-filename-regex='/.cargo' \
  --ignore-filename-regex='/.rustup' \
  --format html \
  -o prof-out/html
