#!/usr/bin/env bash
set -xe

# TODO: less invasive on host system

# TODO: kill server on exit
#trap "kill -- -$$" EXIT

SELFDIR="$(realpath "$(dirname "$0")")"
cd "$SELFDIR"

./setup.sh

cat <<EOF >storage/registered.json
[
  {
    "mac": [82, 84, 0, 18, 52, 86],
    "group": 0,
    "row": 1,
    "col": 1,
    "curr_action": null,
    "curr_progress": null,
    "next_action": "shutdown",
    "image": "contestant"
  }
]
EOF

if ! ip link show br-pixie; then
    sudo ip link add br-pixie type bridge
    sudo ip link set dev br-pixie up
    sudo ip addr add 10.0.0.1/8 brd 10.255.255.255 dev br-pixie
fi

sudo RUST_BACKTRACE=short RUST_LOG=debug ./pixie-server/target/debug/pixie-server &

run_qemu() {
    sudo qemu-system-x86_64 \
        -nographic \
        -enable-kvm \
        -cpu host -smp cores=2 \
        -m 1G \
        -drive if=pflash,format=raw,file=/usr/share/OVMF/OVMF_CODE_4M.fd \
        -drive file=disk.img,if=none,id=nvm,format=raw \
        -device nvme,serial=deadbeef,drive=nvm \
        -nic bridge,mac=52:54:00:12:34:56,br=br-pixie,model=virtio-net-pci
}

truncate -s 8G disk.img
mkfs.ext4 -F disk.img
DEV=$(sudo losetup --show --find disk.img)
sudo mount $DEV /mnt
echo "hello world" | sudo tee /mnt/hello.txt
sudo umount /mnt
sudo losetup -d $DEV

curl 'http://localhost:8080/admin/curr_action/all/store'
run_qemu

rm -f disk.img
truncate -s 8G disk.img

curl 'http://localhost:8080/admin/curr_action/all/flash'
run_qemu

DEV=$(sudo losetup --show --find disk.img)
sudo mount $DEV /mnt
if [ "$(cat /mnt/hello.txt)" != "hello world" ]; then
    echo "hello.txt does not contain the expected content"
    exit 1
fi
sudo umount /mnt
sudo losetup -d $DEV
