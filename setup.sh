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

pushd pixie-uefi
cargo build $RELEASE_FLAG
[ "$RELEASE" ] && upx --best "target/x86_64-unknown-uefi/${TARGET_DIR}/pixie-uefi.efi"
popd

pushd pixie-web
trunk build $RELEASE_FLAG
popd

pushd pixie-server
cargo build $RELEASE_FLAG
popd

mkdir -p "${STORAGE_DIR}/tftpboot" "${STORAGE_DIR}/images" "${STORAGE_DIR}/chunks" "${STORAGE_DIR}/admin"
cp "pixie-uefi/target/x86_64-unknown-uefi/${TARGET_DIR}/pixie-uefi.efi" "${STORAGE_DIR}/tftpboot/"
cp -r pixie-web/dist/* "${STORAGE_DIR}/admin/"

[ -f "${STORAGE_DIR}/config.yaml" ] || cp pixie-server/example.config.yaml "${STORAGE_DIR}/config.yaml"
