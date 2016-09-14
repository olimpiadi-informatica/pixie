#!/bin/sh

TGT=$1
DRIVE=$2

wipe_all() {
    fdisk $DRIVE << EOF
g
w
EOF
}

wipe_linux() {
    echo linux
}

case $TGT in
    linux) wipe_linux;;
    all) wipe_all;;
    *) echo "Invalid wipe type"; exit 1;;
esac
