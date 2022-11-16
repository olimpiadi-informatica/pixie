#!/bin/bash

set -e

echo Waiting for network...

IP=$(echo $1 | cut -f 3 -d / | cut -f 1 -d :)

for i in $(seq 100)
do
  ping -c 1 -W 1 $IP && exit 0
  sleep 1
done

exit 1
