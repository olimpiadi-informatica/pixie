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

BUILDDIR=build
mkdir -p $BUILDDIR

BR2_VER=2022.08

[ -f ${BUILDDIR}/buildroot-${BR2_VER}.tar.xz ] || wget https://buildroot.org/downloads/buildroot-${BR2_VER}.tar.xz -O ${BUILDDIR}/buildroot-${BR2_VER}.tar.xz

[ -d ${BUILDDIR}/buildroot-${BR2_VER} ]  || tar xf ${BUILDDIR}/buildroot-${BR2_VER}.tar.xz -C ${BUILDDIR}

if ! [ -f storage/httpstatic/bzImage ]
then
  unset LD_LIBRARY_PATH
  tar c --exclude-vcs --exclude-vcs-ignores . | gzip > buildroot/pixie.tar.gz
  make O=$PWD/${BUILDDIR}/buildroot-build BR2_EXTERNAL=$PWD/buildroot/ -C ${BUILDDIR}/buildroot-${BR2_VER} pixie2_defconfig 
  make O=$PWD/${BUILDDIR}/buildroot-build -C ${BUILDDIR}/buildroot-${BR2_VER} -j$(nproc)
  cp ${BUILDDIR}/buildroot-build/images/bzImage storage/httpstatic/bzImage
fi
