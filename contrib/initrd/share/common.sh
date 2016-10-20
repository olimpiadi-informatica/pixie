my_umount() {
    mount | grep \ $1\ > /dev/null && umount $1
    return 0
}

get_partition_prefix() {
    DRIVEPP=$1
    [ $DRIVEPP = /dev/nvme0n1 ] && DRIVEPP=/dev/nvme0n1p
    echo -n $DRIVEPP
}

detect_drive() {
    DRIVE=/dev/sda
    [ -e /dev/nvme0n1 ] && DRIVE=/dev/nvme0n1
    echo -n $DRIVE
}

PIXIE_MAGIC=9340f24404c93f7e5a25f69ef1ed5d87791da8a7a0ea1851967c354c572dfcfadb72f2dc5ea2ec7922bfa5b5bce93d0c50ed0977bf899f6e988a991e926466db

error() {
    echo $1
    while true
    do
        /bin/sh
    done
}

mount_pixie() {
    DRIVE=$1
    DRIVEPP=$(get_partition_prefix $DRIVE)
    mount ${DRIVEPP}5 /pixie || return 1
    echo $PIXIE_MAGIC > /tmp/pixie_magic || return 1
    diff /tmp/pixie_magic /pixie/pixie_magic &> /dev/null || return 1
    return 0
}

check_part_size() {
    PART=$1
    MINSZ=$2
    # TODO: get the correct block size (?)
    BLOCKSIZE=512
    # TODO: everything. Check /proc/partitions (?)
    return 0
}
