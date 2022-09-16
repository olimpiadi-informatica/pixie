#!/bin/bash
set -xe

cd "$(realpath "$(dirname "$0")")"

pushd uefi-reboot-skip
cargo +nightly build --release --target x86_64-unknown-uefi
popd

mkdir -p storage/httpstatic
cp uefi-reboot-skip/target/x86_64-unknown-uefi/release/uefi_app.efi storage/httpstatic/reboot.efi

mkdir -p storage/tftpboot
[ -f storage/tftpboot/ipxe.efi ] || wget https://boot.ipxe.org/ipxe.efi -O storage/tftpboot/ipxe.efi

mkdir -p storage/images storage/chunks

[ -f storage/config.yaml ] || cp pixie-core/example.config.yaml storage/config.yaml
