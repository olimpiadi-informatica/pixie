#!/bin/bash
set -xe

SELFDIR="$(realpath "$(dirname "$0")")"

STORAGE_DIR="$1"
STORAGE_DIR="${STORAGE_DIR:=$SELFDIR/storage}"

cd "$SELFDIR"

pushd pixie-uefi
cargo build --release
upx --best target/x86_64-unknown-uefi/release/pixie-uefi.efi
popd

pushd pixie-web
trunk build --release
popd

pushd pixie-server
cargo build --release
popd

mkdir -p "${STORAGE_DIR}/tftpboot"
cp pixie-uefi/target/x86_64-unknown-uefi/release/pixie-uefi.efi "${STORAGE_DIR}/tftpboot/"
mkdir -p "${STORAGE_DIR}/admin"
cp -r pixie-web/dist/* "${STORAGE_DIR}/admin/"
cp pixie-web/style.css "${STORAGE_DIR}/admin/"
cp pixie-web/favicon.ico "${STORAGE_DIR}/admin/"

mkdir -p "${STORAGE_DIR}/images" "${STORAGE_DIR}/chunks"

[ -f "${STORAGE_DIR}/config.yaml" ] || cp pixie-server/example.config.yaml "${STORAGE_DIR}/config.yaml"
