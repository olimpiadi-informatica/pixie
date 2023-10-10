#!/bin/bash

DIR=/var/local/lib/pixie/storage

mkdir -p $DIR

./setup.sh $DIR

cp pixie-core/target/release/pixie-server /usr/local/bin
cp pixie.service /etc/systemd/system/
