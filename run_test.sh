#!/bin/bash
set -xe

TEMPDIR=$(mktemp -d)
DEV=""
# Optional: path to write e2e store/flash timings to, in github-action-benchmark's
# "customSmallerIsBetter" format. Left unset, nothing is timed or written.
BENCH_JSON="$2"

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
cp test_config.yaml $TEMPDIR/storage/config.yaml

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
  },
  {
    "mac": [82, 84, 0, 18, 52, 87],
    "group": 10,
    "row": 9,
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
  ip addr add 10.0.0.1/16 dev br-pixie
fi

if ! ip link show br-pixie1; then
  ip link add br-pixie1 type bridge
  ip link set dev br-pixie1 up
  ip addr add 10.10.0.1/16 dev br-pixie1
fi

RUST_BACKTRACE=short RUST_LOG=debug RUST_LOG_STYLE=always LLVM_PROFILE_FILE=prof-out/pixie-server-%m-%p.profraw ./pixie-server/target/debug/pixie-server -s $TEMPDIR/storage &

run_qemu() {
  OVMF=/usr/share/OVMF/OVMF_CODE_4M.fd
  if ! [ -e $OVMF ]; then
    OVMF=/usr/share/edk2/x64/OVMF_CODE.4m.fd
  fi
  FILE=prof-out/pixie-uefi-$RANDOM.profraw
  truncate -s 500M $FILE
  # A guest CPU exception leaves QEMU running forever (OVMF dead-loops after
  # dumping the register state), so bound this instead of relying on the
  # much coarser CI job timeout to eventually kill it.
  #
  # stdin is explicitly /dev/null: -chardev stdio only needs stdout to log the
  # guest's serial console. If this runs as a background job of an
  # interactive shell (e.g. a watch-and-rerun loop), inheriting the real
  # controlling terminal on stdin makes QEMU's raw-mode tcsetattr() draw a
  # SIGTTOU the instant it starts, which silently stops the process (not a
  # crash, no output) until the 90s timeout kills it.
  timeout -k 10 90 qemu-system-x86_64 \
    -nographic \
    -chardev stdio,id=char0,logfile=$1,signal=off \
    -serial chardev:char0 \
    -monitor none \
    -enable-kvm \
    -cpu host -smp cores=2 \
    -m 1G \
    -drive if=pflash,format=raw,file=$OVMF \
    -drive file=$TEMPDIR/disk.img,if=none,id=nvm,format=raw \
    -drive file=$FILE,if=none,id=cov,format=raw \
    -device nvme,serial=deadbeef,drive=nvm \
    -device nvme,serial=covdrive,drive=cov \
    -nic bridge,mac=52:54:00:12:34:56,br=br-pixie,model=e1000 \
    </dev/null
}

run_qemu1() {
  OVMF=/usr/share/OVMF/OVMF_CODE_4M.fd
  if ! [ -e $OVMF ]; then
    OVMF=/usr/share/edk2/x64/OVMF_CODE.4m.fd
  fi
  FILE=prof-out/pixie-uefi-$RANDOM.profraw
  truncate -s 500M $FILE
  # See run_qemu() above for why this is bounded with a timeout.
  timeout -k 10 90 qemu-system-x86_64 \
    -nographic \
    -chardev stdio,id=char0,logfile=$1,signal=off \
    -serial chardev:char0 \
    -monitor none \
    -enable-kvm \
    -cpu host -smp cores=2 \
    -m 1G \
    -drive if=pflash,format=raw,file=$OVMF \
    -drive file=$TEMPDIR/disk1.img,if=none,id=nvm,format=raw \
    -drive file=$FILE,if=none,id=cov,format=raw \
    -device nvme,serial=deadbeef,drive=nvm \
    -device nvme,serial=covdrive,drive=cov \
    -nic bridge,mac=52:54:00:12:34:57,br=br-pixie1,model=e1000 \
    </dev/null
}

truncate -s 8G $TEMPDIR/disk.img
truncate -s 8G $TEMPDIR/disk1.img
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

curl 'http://localhost:8080/admin/curr_action/mac:52:54:00:12:34:56/store'
store_start=$(date +%s.%N)
run_qemu $TEMPDIR/store.log
store_end=$(date +%s.%N)

# Check that we restore the original disk image.

rm -f $TEMPDIR/disk.img
truncate -s 8G $TEMPDIR/disk.img

curl 'http://localhost:8080/admin/curr_action/all/flash'
flash_cold_start=$(date +%s.%N)
run_qemu $TEMPDIR/flash-1.log
flash_cold_end=$(date +%s.%N)
run_qemu1 $TEMPDIR/flash1-1.log

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

DEV=$(losetup --partscan --show --find --read-only $TEMPDIR/disk1.img)
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
flash_cached_start=$(date +%s.%N)
run_qemu $TEMPDIR/flash-2.log
flash_cached_end=$(date +%s.%N)
run_qemu1 $TEMPDIR/flash1-2.log

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

DEV=$(losetup --partscan --show --find --read-only $TEMPDIR/disk1.img)
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

if ! grep "Disk scanned; 0 chunks to fetch" $TEMPDIR/flash1-2.log &>/dev/null; then
  echo "Data was re-fetched"
  exit 1
fi

# Only reached if every correctness check above passed.
if [ -n "$BENCH_JSON" ]; then
  awk -v s0="$store_start" -v s1="$store_end" \
      -v c0="$flash_cold_start" -v c1="$flash_cold_end" \
      -v k0="$flash_cached_start" -v k1="$flash_cached_end" \
      'BEGIN {
        printf "[\n"
        printf "  {\"name\": \"store (qemu e2e)\", \"unit\": \"s\", \"value\": %.3f},\n", s1 - s0
        printf "  {\"name\": \"flash, cold (qemu e2e)\", \"unit\": \"s\", \"value\": %.3f},\n", c1 - c0
        printf "  {\"name\": \"flash, cached (qemu e2e)\", \"unit\": \"s\", \"value\": %.3f}\n", k1 - k0
        printf "]\n"
      }' >"$BENCH_JSON"
fi
