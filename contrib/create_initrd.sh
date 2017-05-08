#!/bin/bash -e

KERNEL_VERSION=4.11
BUSYBOX_VERSION=1.26.2
wget -c "https://cdn.kernel.org/pub/linux/kernel/v4.x/linux-""$KERNEL_VERSION"".tar.xz"
wget -c "http://busybox.net/downloads/busybox-""$BUSYBOX_VERSION"".tar.bz2"
tar xfv "busybox-""$BUSYBOX_VERSION"".tar.bz2"
tar xfv "linux-""$KERNEL_VERSION"".tar.xz"
cp "busybox-""$BUSYBOX_VERSION"".config" "busybox-""$BUSYBOX_VERSION""/.config"
cp "linux-""$KERNEL_VERSION"".config" "linux-""$KERNEL_VERSION""/.config"
cd "busybox-""$BUSYBOX_VERSION"
make -j $((`nproc`+1))
cd ..
cp "busybox-""$BUSYBOX_VERSION""/busybox" "initrd/bin/"
cd "linux-""$KERNEL_VERSION"
make -j $((`nproc`+1))
cp arch/x86/boot/bzImage ..
cd ..
cd stardust
cargo build --release --target=x86_64-unknown-linux-musl
cd ..
cp stardust/target/x86_64-unknown-linux-musl/release/stardust initrd/bin/
mkdir -p build
cd build
cmake -DBUILD_KEXEC=ON ../..
make -j $((`nproc`+1))
cd ..
cp build/kexec initrd/bin/
strip -s initrd/bin/stardust
strip -s initrd/bin/kexec
strip -s initrd/bin/busybox
cd initrd
LVL=$1
[ -z "$LVL" ] && LVL=9
find ./ | cpio -H newc -o | xz -C crc32 --x86 -e -$LVL > ../initrd.img
