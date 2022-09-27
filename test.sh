#!/bin/sh

set -xe
cd "$(realpath "$(dirname "$0")")"

function cleanup() {
	cd /tmp/pixie

	[[ -z "$LOOPA" ]] || {
		sudo umount a/mnt
		sudo losetup -d "$LOOPA"
	}

	[[ -z "$LOOPB" ]] || {
		sudo umount b/mnt
		sudo losetup -d "$LOOPB"
	}
}

trap cleanup EXIT

pushd pixie-push
cargo build
PUSH="$(realpath ./target/debug/pixie-push)"
popd

pushd pixie-pull
cargo build
PULL="$(realpath ./target/debug/pixie-pull)"
popd

mkdir -p /tmp/pixie/a/mnt /tmp/pixie/b/mnt
cd /tmp/pixie

cd a
rm -f disk.img
truncate -s 1073741824 disk.img

fdisk disk.img <<EOF
g
n



w
EOF

LOOPA=$(sudo losetup -o 1MiB --sizelimit 1071644672 -f --show disk.img)
sudo mkfs.ext4 "$LOOPA"
sudo mount "$LOOPA" mnt

cd mnt
sudo sh -c "cat > main.cpp" <<EOF
#include <iostream>

int main() {
	std::cout << "Hello World\n";
}
EOF
sudo g++ -o main main.cpp -fsanitize=address,undefined -g
cd ..

sudo umount mnt
sudo losetup -d "$LOOPA"
unset LOOPA

"$PUSH" -d /tmp/pixie/img -- disk.img
cd ..

cd b
"$PULL" -s /tmp/pixie/img

LOOPB=$(sudo losetup -o 1MiB --sizelimit 1071644672 -f --show disk.img)
sudo mount "$LOOPB" mnt

cd mnt
[[ -f main.cpp ]]
[[ -f main ]]
[[ "$(./main)" = "Hello World" ]]
cd ..

sudo umount mnt
sudo losetup -d "$LOOPB"
unset LOOPB

cd ..
