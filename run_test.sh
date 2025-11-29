#!/bin/bash
set -xe

TEMPDIR=$(mktemp -d)
DEV=""

mkdir $TEMPDIR/mnt

cleanup() {
  # Ignore errors in cleanup.
  set +e
  umount -l -q $TEMPDIR/mnt
  [ -z "$DEV" ] || losetup -d $DEV
  rm -rf $TEMPDIR
  # Kill descendants, but not itself.
  trap '' SIGTERM
  kill -- -$(ps -o pgid= $$ | tr -d ' ')
  trap - SIGTERM
}

trap cleanup EXIT

cp -rv $1 $TEMPDIR

cat >$TEMPDIR/storage/registered.json <<EOF
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

RUST_BACKTRACE=short RUST_LOG=debug RUST_LOG_STYLE=always LLVM_PROFILE_FILE=prof-out/pixie-server-%m-%p.profraw ./pixie-server/target/debug/pixie-server -s $TEMPDIR/storage &

run_qemu() {
  OVMF=/usr/share/OVMF/OVMF_CODE_4M.fd
  if ! [ -e $OVMF ]; then
    OVMF=/usr/share/edk2/x64/OVMF_CODE.4m.fd
  fi
  FILE=prof-out/pixie-uefi-$RANDOM.profraw
  truncate -s 500M $FILE
  qemu-system-x86_64 \
    -nographic \
    -chardev stdio,id=char0,logfile=$1,signal=off \
    -serial chardev:char0 \
    -monitor none \
    -enable-kvm \
    -cpu host -smp cores=2 \
    -m 1G \
    -drive if=pflash,format=raw,file=$OVMF \
    -drive file=$TEMPDIR/disk.img,if=none,id=nvm,format=raw \
    -drive file=$FILE,format=raw \
    -device nvme,serial=deadbeef,drive=nvm \
    -nic bridge,mac=52:54:00:12:34:56,br=br-pixie,model=e1000
}

truncate -s 8G $TEMPDIR/disk.img
echo -e "label: gpt\n- 1GiB - -\n- 1GiB - -\n- - L -" | sfdisk $TEMPDIR/disk.img
DEV=$(losetup --partscan --show --find $TEMPDIR/disk.img)
mkswap ${DEV}p1
mkfs.ntfs -f ${DEV}p2
mkfs.ext4 ${DEV}p3
for PART in ${DEV}p{2..3}; do
  mount $PART $TEMPDIR/mnt
  cp ./pixie-server/target/debug/pixie-server $TEMPDIR/mnt
  umount $TEMPDIR/mnt
done
losetup -d $DEV

curl 'http://localhost:8080/admin/curr_action/all/store'
run_qemu $TEMPDIR/store.log

# Check that we restore the original disk image.

rm -f $TEMPDIR/disk.img
truncate -s 8G $TEMPDIR/disk.img

curl 'http://localhost:8080/admin/curr_action/all/flash'
run_qemu $TEMPDIR/flash-1.log

DEV=$(losetup --partscan --show --find --read-only $TEMPDIR/disk.img)
fsck -n ${DEV}p*
for PART in ${DEV}p{2..3}; do
  mount -o ro $PART $TEMPDIR/mnt
  if [ "$(md5sum $TEMPDIR/mnt/pixie-server | cut -f 1 -d ' ')" != "$(md5sum ./pixie-server/target/debug/pixie-server | cut -f 1 -d ' ')" ]; then
    echo "pixie-server does not contain the expected content"
    exit 1
  fi
  umount $TEMPDIR/mnt
done
losetup -d $DEV

# Check that we don't fetch any data if the disk contents have not changed.

curl 'http://localhost:8080/admin/curr_action/all/flash'
run_qemu $TEMPDIR/flash-2.log

DEV=$(losetup --partscan --show --find --read-only $TEMPDIR/disk.img)
fsck -n ${DEV}p*
for PART in ${DEV}p{2..3}; do
  mount -o ro $PART $TEMPDIR/mnt
  if [ "$(md5sum $TEMPDIR/mnt/pixie-server | cut -f 1 -d ' ')" != "$(md5sum ./pixie-server/target/debug/pixie-server | cut -f 1 -d ' ')" ]; then
    echo "pixie-server does not contain the expected content"
    exit 1
  fi
  umount $TEMPDIR/mnt
done
losetup -d $DEV

if ! grep "Disk scanned; 0 chunks to fetch" $TEMPDIR/flash-2.log &>/dev/null; then
  echo "Data was re-fetched"
  exit 1
fi
