#!/usr/bin/env bash
set -xe

SELFDIR="$(realpath "$(dirname "$0")")"
cd "$SELFDIR"

STORAGE_DIR="${SELFDIR}/storage"
RELEASE=""

while [[ $# -gt 0 ]]; do
  case $1 in
    --release)
      RELEASE=1
      shift
      ;;
    -*|--*)
      echo "Unknown option $1"
      exit 1
      ;;
    *)
      STORAGE_DIR="$1"
      shift # past argument
      ;;
  esac
done

RELEASE_FLAG=""
[ "$RELEASE" ] && RELEASE_FLAG="--release"

TARGET_DIR="debug"
[ "$RELEASE" ] && TARGET_DIR="release"

pushd pixie-client
if [ "$RELEASE" ]; then
  SYSROOT="$(rustc --print sysroot)"
  COMMIT_HASH="$(rustc -Vv | awk '/commit-hash:/ {print $2}')"
  REMAP_FLAGS="--remap-path-prefix=${SYSROOT}/lib/rustlib/src/rust/library/= --remap-path-prefix=/rustc/${COMMIT_HASH}/library/= --remap-path-prefix=${SELFDIR}/="
  for reg_dir in "${CARGO_HOME:-$HOME/.cargo}"/registry/src/*/; do
    [ -d "$reg_dir" ] && REMAP_FLAGS="${REMAP_FLAGS} --remap-path-prefix=${reg_dir}="
  done
  CARGO_TARGET_X86_64_UNKNOWN_UEFI_RUSTFLAGS="${CARGO_TARGET_X86_64_UNKNOWN_UEFI_RUSTFLAGS:+$CARGO_TARGET_X86_64_UNKNOWN_UEFI_RUSTFLAGS }${REMAP_FLAGS}" \
    cargo build $RELEASE_FLAG
  upx --lzma --best "target/x86_64-unknown-uefi/${TARGET_DIR}/pixie-client.efi"
else
  cargo build $RELEASE_FLAG
fi
popd

pushd pixie-web
trunk build $RELEASE_FLAG
popd

pushd pixie-server
cargo build $RELEASE_FLAG
popd

mkdir -p "${STORAGE_DIR}/tftpboot" "${STORAGE_DIR}/images" "${STORAGE_DIR}/chunks" "${STORAGE_DIR}/admin"
cp "pixie-client/target/x86_64-unknown-uefi/${TARGET_DIR}/pixie-client.efi" "${STORAGE_DIR}/tftpboot/"
cp -r pixie-web/dist/* "${STORAGE_DIR}/admin/"

[ -f "${STORAGE_DIR}/config.yaml" ] || cp pixie-server/example.config.yaml "${STORAGE_DIR}/config.yaml"
