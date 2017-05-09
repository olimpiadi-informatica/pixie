#!/bin/bash -e

KERNEL_VERSION=4.11
BUILDROOT_VERSION=2017.02.2

pushd ipxe/src
make -j $((`nproc`+1)) bin/undionly.kpxe EMBED=../../bootp_only.ipxe
cp bin/undionly.kpxe ../..
popd

if [ ! -d "linux-${KERNEL_VERSION}" ]
then
    wget -c "https://cdn.kernel.org/pub/linux/kernel/v4.x/linux-${KERNEL_VERSION}.tar.xz"
    tar xfv "linux-${KERNEL_VERSION}.tar.xz"
    cp "linux-${KERNEL_VERSION}.config" "linux-${KERNEL_VERSION}/.config"
fi
pushd "linux-${KERNEL_VERSION}"
make -j $((`nproc`+1))
cp arch/x86/boot/bzImage ../vmlinuz.img
popd

if [ ! -d "buildroot-${BUILDROOT_VERSION}" ]
then
    wget -c "https://buildroot.org/downloads/buildroot-${BUILDROOT_VERSION}.tar.bz2"
    tar xfv "buildroot-$BUILDROOT_VERSION.tar.bz2"
    cp "buildroot-${BUILDROOT_VERSION}.config" "buildroot-${BUILDROOT_VERSION}/.config"
    cp busybox.config "buildroot-${BUILDROOT_VERSION}/package/busybox/busybox.config"
fi
pushd "buildroot-${BUILDROOT_VERSION}"
make
popd

pushd stardust
cargo build --release --target=x86_64-unknown-linux-musl
popd

pushd ..
make -j $((`nproc`+1)) CC="contrib/buildroot-${BUILDROOT_VERSION}/output/host/usr/bin/x86_64-buildroot-linux-musl-gcc"
popd

cp stardust/target/x86_64-unknown-linux-musl/release/stardust initrd/bin/
cp ../build/kexec initrd/bin/
cp "buildroot-${BUILDROOT_VERSION}/output/target/bin/busybox" initrd/bin/
cp "buildroot-${BUILDROOT_VERSION}/output/target/usr/sbin/mke2fs" initrd/bin/mkfs.ext4

pushd initrd
LVL=$1
[ -z "$LVL" ] && LVL=9
find ./ | cpio -H newc -o | xz -C crc32 --x86 -e -$LVL > ../initrd.img
popd

mkdir -p doconfig/bin/
cp ../build/tinycurl doconfig/bin
cp ../build/reboot doconfig/bin
cp "buildroot-${BUILDROOT_VERSION}/output/target/bin/busybox" doconfig/bin/
cp "buildroot-${BUILDROOT_VERSION}/output/target/usr/bin/dialog" doconfig/bin/

pushd doconfig
LVL=$1
[ -z "$LVL" ] && LVL=9
find ./ | cpio -H newc -o | xz -C crc32 --x86 -e -$LVL > ../doconfig.img
popd
