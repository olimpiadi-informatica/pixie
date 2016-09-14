#!/bin/sh
cd initrd
LVL=$1
[ -z "$LVL" ] && LVL=9
find ./ | cpio -H newc -o | xz -C crc32 --x86 -e -$LVL > ../initrd.img
