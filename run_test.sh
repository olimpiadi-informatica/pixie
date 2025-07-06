#!/bin/bash
set -xe

TEMPDIR=$(mktemp -d)

mkdir $TEMPDIR/mnt

cleanup() {
  # Ignore errors in cleanup.
  set +e
  umount -l -q $TEMPDIR/mnt
  rm -rf $TEMPDIR
  # Kill descendants, but not itself.
  trap '' SIGTERM
  kill -- -$(ps -o pgid= $$ | tr -d ' ')
  trap - SIGTERM
}

trap cleanup EXIT

cp -rv $1 $TEMPDIR

cat > $TEMPDIR/storage/registered.json << EOF
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
    ip link add br-pixie type bridge
    ip link set dev br-pixie up
    ip addr add 10.0.0.1/8 brd 10.255.255.255 dev br-pixie
fi

RUST_BACKTRACE=short RUST_LOG=debug ./pixie-server/target/debug/pixie-server -s $TEMPDIR/storage &

run_qemu() {
    OVMF=/usr/share/OVMF/OVMF_CODE_4M.fd
    if ! [ -e $OVMF ]
    then
      OVMF=/usr/share/edk2/x64/OVMF_CODE.4m.fd
    fi
    qemu-system-x86_64 \
        -nographic \
        -enable-kvm \
        -cpu host -smp cores=2 \
        -m 1G \
        -drive if=pflash,format=raw,file=$OVMF \
        -drive file=$TEMPDIR/disk.img,if=none,id=nvm,format=raw \
        -device nvme,serial=deadbeef,drive=nvm \
        -nic bridge,mac=52:54:00:12:34:56,br=br-pixie,model=virtio-net-pci
}

truncate -s 8G $TEMPDIR/disk.img
mkfs.ext4 $TEMPDIR/disk.img
DEV=$(losetup --show --find $TEMPDIR/disk.img)
mount $DEV $TEMPDIR/mnt
cp ./pixie-server/target/debug/pixie-server $TEMPDIR/mnt
umount $TEMPDIR/mnt
losetup -d $DEV

curl 'http://localhost:8080/admin/curr_action/all/store'
run_qemu

rm -f $TEMPDIR/disk.img
truncate -s 8G $TEMPDIR/disk.img

curl 'http://localhost:8080/admin/curr_action/all/flash'
run_qemu

DEV=$(losetup --show --find $TEMPDIR/disk.img)
mount $DEV $TEMPDIR/mnt
if [ "$(md5sum $TEMPDIR/mnt/pixie-server | cut -f 1 -d ' ')" != "$(md5sum ./pixie-server/target/debug/pixie-server | cut -f 1 -d ' ')" ]; then
    echo "pixie-server does not contain the expected content"
    exit 1
fi
umount $TEMPDIR/mnt
losetup -d $DEV
