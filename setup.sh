#!/bin/bash
set -xe

cd "$(realpath "$(dirname "$0")")"

pushd pixie-uefi
cargo build --release
upx --best target/x86_64-unknown-uefi/release/uefi_app.efi
popd

pushd pixie-core
cargo build --release
popd

mkdir -p storage/tftpboot
cp pixie-uefi/target/x86_64-unknown-uefi/release/uefi_app.efi storage/tftpboot/

mkdir -p storage/images storage/chunks

[ -f storage/config.yaml ] || cp pixie-core/example.config.yaml storage/config.yaml
