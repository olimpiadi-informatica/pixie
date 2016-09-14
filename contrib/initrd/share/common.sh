get_partition_prefix() {
    DRIVEPP=$1
    [ $DRIVEPP -eq /dev/nvme0n1 ] && DRIVEPP=/dev/nvme0n1p
    echo -n $DRIVEPP
}

detect_drive() {
    DRIVE=/dev/sda
    [ -e /dev/nvme0n1 ] && DRIVE=/dev/nvme0n1
    echo -n $DRIVE
}

PIXIE_MAGIC=9340f24404c93f7e5a25f69ef1ed5d87791da8a7a0ea1851967c354c572dfcfadb72f2dc5ea2ec7922bfa5b5bce93d0c50ed0977bf899f6e988a991e926466db
