#!/bin/bash
set -xe

TEMPDIR=$(mktemp -d)
export XDG_RUNTIME_DIR="${XDG_RUNTIME_DIR:-$TEMPDIR}"
# Optional: path to write e2e store/flash timings to, in github-action-benchmark's
# "customSmallerIsBetter" format. Left unset, nothing is timed or written.
BENCH_JSON="$2"
QEMU_DISPLAY_ARGS="${QEMU_DISPLAY_ARGS:--nographic}"

cleanup() {
  # Ignore errors in cleanup.
  set +e
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
    "image": "contestant",
    "stats": {"ram": 0, "disk": 0, "cpu": "UNKNOWN"}
  },
  {
    "mac": [82, 84, 0, 18, 52, 87],
    "group": 10,
    "row": 9,
    "col": 1,
    "curr_action": null,
    "curr_progress": null,
    "next_action": "shutdown",
    "image": "contestant",
    "stats": {"ram": 0, "disk": 0, "cpu": "UNKNOWN"}
  }
]
EOF

if ! ip link show br-pixie &>/dev/null; then
  ip link add br-pixie type bridge
  ip link set dev br-pixie up
  ip addr add 10.0.0.1/16 dev br-pixie
fi

if ! ip link show tap-pixie0 &>/dev/null; then
  ip tuntap add dev tap-pixie0 mode tap
  ip link set tap-pixie0 master br-pixie
  ip link set tap-pixie0 up
fi

if ! ip link show br-pixie1 &>/dev/null; then
  ip link add br-pixie1 type bridge
  ip link set dev br-pixie1 up
  ip addr add 10.10.0.1/16 dev br-pixie1
  ip addr add 192.168.99.1/24 dev br-pixie1
  ip addr add 192.168.99.2/24 dev br-pixie1
fi

if ! ip link show tap-pixie1 &>/dev/null; then
  ip tuntap add dev tap-pixie1 mode tap
  ip link set tap-pixie1 master br-pixie1
  ip link set tap-pixie1 up
fi

# External DHCP server on br-pixie1 (192.168.99.2) that assigns 192.168.99.50/24
# with no PXE/bootfile options, so br-pixie1 tests DhcpMode::Proxy end-to-end.
python3 - <<'PY' &
import socket, struct

sock = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
sock.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
sock.setsockopt(socket.SOL_SOCKET, socket.SO_BROADCAST, 1)
sock.setsockopt(socket.SOL_SOCKET, socket.SO_BINDTODEVICE, b"br-pixie1")
sock.bind(("0.0.0.0", 67))

SERVER_IP = socket.inet_aton("192.168.99.2")
CLIENT_IP = socket.inet_aton("192.168.99.50")
NETMASK = socket.inet_aton("255.255.255.0")
COOKIE = b"\x63\x82\x53\x63"

while True:
    pkt, _ = sock.recvfrom(2048)
    if len(pkt) < 240 or pkt[0] != 1 or pkt[236:240] != COOKIE:
        continue
    opts = pkt[240:]
    msg_type = None
    server_id = None
    i = 0
    while i < len(opts) and opts[i] != 255:
        if opts[i] == 0:
            i += 1
            continue
        code, length = opts[i], opts[i + 1]
        val = opts[i + 2 : i + 2 + length]
        if code == 53 and length == 1:
            msg_type = val[0]
        elif code == 54 and length == 4:
            server_id = val
        i += 2 + length
    if msg_type == 1:
        reply_type = 2  # OFFER
    elif msg_type == 3 and (server_id is None or server_id == SERVER_IP):
        reply_type = 5  # ACK
    else:
        continue
    reply = bytearray(240)
    reply[0] = 2  # BOOTREPLY
    reply[1:4] = pkt[1:4]
    reply[4:8] = pkt[4:8]  # xid
    reply[10:12] = pkt[10:12]  # flags
    reply[16:20] = CLIENT_IP  # yiaddr
    reply[28:44] = pkt[28:44]  # chaddr
    reply[236:240] = COOKIE
    reply += bytes([53, 1, reply_type])
    reply += bytes([54, 4]) + SERVER_IP
    reply += bytes([51, 4]) + struct.pack("!I", 3600)
    reply += bytes([1, 4]) + NETMASK
    reply += bytes([3, 4]) + SERVER_IP
    reply += bytes([255])
    sock.sendto(reply, ("255.255.255.255", 68))
PY

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
    $QEMU_DISPLAY_ARGS \
    -chardev stdio,id=char0,logfile=$1,signal=off \
    -serial chardev:char0 \
    -monitor none \
    -enable-kvm \
    -cpu host -smp cores=2 \
    -m 1G \
    -drive if=pflash,format=raw,readonly=on,file=$OVMF \
    -drive file=$TEMPDIR/disk.img,if=none,id=nvm,format=raw \
    -drive file=$FILE,if=none,id=cov,format=raw \
    -device nvme,serial=deadbeef,drive=nvm \
    -device nvme,serial=covdrive,drive=cov \
    -netdev tap,id=net0,ifname=tap-pixie0,script=no,downscript=no \
    -device e1000,netdev=net0,mac=52:54:00:12:34:56 \
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
    $QEMU_DISPLAY_ARGS \
    -chardev stdio,id=char0,logfile=$1,signal=off \
    -serial chardev:char0 \
    -monitor none \
    -enable-kvm \
    -cpu host -smp cores=2 \
    -m 1G \
    -drive if=pflash,format=raw,readonly=on,file=$OVMF \
    -drive file=$TEMPDIR/disk1.img,if=none,id=nvm,format=raw \
    -drive file=$FILE,if=none,id=cov,format=raw \
    -device nvme,serial=deadbeef,drive=nvm \
    -device nvme,serial=covdrive,drive=cov \
    -netdev tap,id=net0,ifname=tap-pixie1,script=no,downscript=no \
    -device e1000,netdev=net0,mac=52:54:00:12:34:57 \
    </dev/null
}

# Partition layout created by sfdisk on an 8 GiB GPT disk:
#   p1 (swap): start sector 2048    (1 MiB),    size 2097152 sectors (1 GiB)
#   p2 (ntfs): start sector 2099200 (1025 MiB), size 2097152 sectors (1 GiB)
#   p3 (ext4): start sector 4196352 (2049 MiB), size 12578816 sectors (6142 MiB)
P1_OFFSET=$((2048 * 512))
P1_SIZE=$((2097152 * 512))
P2_OFFSET=$((2099200 * 512))
P2_SIZE=$((2097152 * 512))
P3_OFFSET=$((4196352 * 512))
P3_SIZE=$((12578816 * 512))

EXPECTED_MD5=$(md5sum ./pixie-server/target/debug/pixie-server | cut -f 1 -d ' ')

truncate -s 8G $TEMPDIR/disk.img
truncate -s 8G $TEMPDIR/disk1.img
echo -e "label: gpt\n- 1GiB - -\n- 1GiB - -\n- - L -" | sfdisk $TEMPDIR/disk.img

# Format p1 (swap) rootlessly and splice into disk.img
truncate -s $P1_SIZE $TEMPDIR/p1.swap
mkswap $TEMPDIR/p1.swap
dd if=$TEMPDIR/p1.swap of=$TEMPDIR/disk.img bs=1M seek=1 conv=notrunc,sparse status=none
rm -f $TEMPDIR/p1.swap

# Format p2 (NTFS) rootlessly, copy pixie-server into it, and splice into disk.img
truncate -s $P2_SIZE $TEMPDIR/p2.ntfs
mkfs.ntfs -Q -F $TEMPDIR/p2.ntfs
ntfscp $TEMPDIR/p2.ntfs ./pixie-server/target/debug/pixie-server pixie-server
dd if=$TEMPDIR/p2.ntfs of=$TEMPDIR/disk.img bs=1M seek=1025 conv=notrunc,sparse status=none
rm -f $TEMPDIR/p2.ntfs

# Format p3 (ext4) directly at P3_OFFSET in disk.img populated from $TEMPDIR/root
mkdir -p $TEMPDIR/root
cp ./pixie-server/target/debug/pixie-server $TEMPDIR/root/pixie-server
mkfs.ext4 -q -F -E offset=$P3_OFFSET -d $TEMPDIR/root $TEMPDIR/disk.img "$((P3_SIZE / 1024))K"

verify_disk() {
  local img="$1"
  # Verify p1 (swap)
  if [ "$(blkid -p --offset "$P1_OFFSET" --size "$P1_SIZE" -s TYPE -o value "$img")" != "swap" ]; then
    echo "Swap partition on $img is not a valid swap partition"
    exit 1
  fi

  # Verify p2 (NTFS)
  dd if="$img" of=$TEMPDIR/check_p2.ntfs bs=1M skip=1025 count=1024 status=none
  if [ "$(ntfscat $TEMPDIR/check_p2.ntfs pixie-server | md5sum | cut -f 1 -d ' ')" != "$EXPECTED_MD5" ]; then
    echo "NTFS partition on $img does not contain the expected pixie-server content"
    exit 1
  fi
  rm -f $TEMPDIR/check_p2.ntfs

  # Verify p3 (ext4)
  e2fsck -fn "$img?offset=$P3_OFFSET"
  if [ "$(debugfs -R "cat pixie-server" "$img?offset=$P3_OFFSET" 2>/dev/null | md5sum | cut -f 1 -d ' ')" != "$EXPECTED_MD5" ]; then
    echo "ext4 partition on $img does not contain the expected pixie-server content"
    exit 1
  fi
}

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

verify_disk $TEMPDIR/disk.img
verify_disk $TEMPDIR/disk1.img

# Check that we don't fetch any data if the disk contents have not changed.

curl 'http://localhost:8080/admin/curr_action/all/flash'
flash_cached_start=$(date +%s.%N)
run_qemu $TEMPDIR/flash-2.log
flash_cached_end=$(date +%s.%N)
run_qemu1 $TEMPDIR/flash1-2.log

verify_disk $TEMPDIR/disk.img
verify_disk $TEMPDIR/disk1.img

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
