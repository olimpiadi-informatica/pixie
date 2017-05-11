CFLAGS:=-Os -static

NPROC:=$(shell nproc)

KERNEL_VERSION:=4.11
BUILDROOT_VERSION:=2017.02.2
CC:=build/buildroot-${BUILDROOT_VERSION}/output/host/usr/bin/x86_64-buildroot-linux-musl-gcc

all: build/target/vmlinuz.img build/target/undionly.kpxe build/target/doconfig.img build/target/initrd.img

build/linux-${KERNEL_VERSION}.tar.xz: 
	cd build && wget -c "https://cdn.kernel.org/pub/linux/kernel/v4.x/linux-${KERNEL_VERSION}.tar.xz"

build/linux-${KERNEL_VERSION}: build/linux-${KERNEL_VERSION}.tar.xz
	cd build && tar xvf "linux-${KERNEL_VERSION}.tar.xz"

build/target/undionly.kpxe: ipxe
	cd ipxe/src && ${MAKE} -j ${NPROC} bin/undionly.kpxe EMBED=../../src/bootp_only.ipxe
	cp ipxe/src/bin/undionly.kpxe build/target/undionly.kpxe

stardust/target/x86_64-unknown-linux-musl/release/stardust: stardust 
	cd stardust && cargo build --release --target=x86_64-unknown-linux-musl
	strip --strip-all stardust/target/x86_64-unknown-linux-musl/release/stardust

build/target/vmlinuz.img: config/linux-${KERNEL_VERSION}.config build/linux-${KERNEL_VERSION}
	cp config/linux-${KERNEL_VERSION}.config build/linux-${KERNEL_VERSION}/.config
	mkdir -p build/target/
	cd build/linux-${KERNEL_VERSION} && ${MAKE} -j ${NPROC}
	cp build/linux-${KERNEL_VERSION}/arch/x86/boot/bzImage build/target/vmlinuz.img

build/buildroot-${BUILDROOT_VERSION}.tar.bz2:
	cd build && wget -c "https://buildroot.org/downloads/buildroot-${BUILDROOT_VERSION}.tar.bz2"

build/buildroot-${BUILDROOT_VERSION}: build/buildroot-${BUILDROOT_VERSION}.tar.bz2
	cd build && tar xvf buildroot-${BUILDROOT_VERSION}.tar.bz2

build/buildroot-${BUILDROOT_VERSION}/output: build/buildroot-${BUILDROOT_VERSION} config/buildroot-${BUILDROOT_VERSION}.config
	cp config/buildroot-${BUILDROOT_VERSION}.config build/buildroot-${BUILDROOT_VERSION}/.config
	cd build/buildroot-${BUILDROOT_VERSION} && ${MAKE}
	touch build/buildroot-${BUILDROOT_VERSION}/output

build/target/initrd.img: stardust/target/x86_64-unknown-linux-musl/release/stardust build/kexec build/buildroot-${BUILDROOT_VERSION}/output src/main_initrd/init src/main_initrd/wipe.sh src/main_initrd/common.sh
	mkdir -p build/initrd/{bin,share}
	rm -f build/initrd/usr
	ln -s . build/initrd/usr
	cp src/main_initrd/init build/initrd
	cp src/main_initrd/wipe.sh build/initrd/bin/
	cp src/main_initrd/common.sh build/initrd/share/
	cp stardust/target/x86_64-unknown-linux-musl/release/stardust build/initrd/bin/
	cp build/kexec build/initrd/bin/
	cp "build/buildroot-${BUILDROOT_VERSION}/output/target/bin/busybox" build/initrd/bin/
	cp "build/buildroot-${BUILDROOT_VERSION}/output/target/usr/sbin/mke2fs" build/initrd/bin/mkfs.ext4
	cd build/initrd && find ./ | cpio -H newc -o | xz -C crc32 --x86 -e -9 > ../target/initrd.img

build/target/doconfig.img: build/reboot build/tinycurl build/buildroot-${BUILDROOT_VERSION}/output src/doconfig/init
	mkdir -p build/doconfig/bin
	cp src/doconfig/init build/doconfig
	cp build/reboot build/doconfig/bin/
	cp build/tinycurl build/doconfig/bin/
	cp "build/buildroot-${BUILDROOT_VERSION}/output/target/bin/busybox" build/doconfig/bin/
	cp "build/buildroot-${BUILDROOT_VERSION}/output/target/usr/bin/dialog" build/doconfig/bin/
	mkdir -p build/doconfig/usr/share/terminfo/l
	ln -sf /bin build/doconfig/usr
	ln -sf /sbin build/doconfig/usr
	cp "build/buildroot-${BUILDROOT_VERSION}/output/target/usr/share/terminfo/l/linux" build/doconfig/usr/share/terminfo/l/
	cd build/doconfig && find ./ | cpio -H newc -o | xz -C crc32 --x86 -e -9 > ../target/doconfig.img

build/%: util/%.c build/buildroot-${BUILDROOT_VERSION}/output
	${CC} ${CFLAGS} $< -o $@
	strip --strip-debug --strip-unneeded $@

clean:
	rm -f build/*
