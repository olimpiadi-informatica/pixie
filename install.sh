#!/usr/bin/env bash

DIR=/var/local/lib/pixie

mkdir -p $DIR

./setup.sh --release $DIR

cp --remove-destination pixie-server/target/release/pixie-server /usr/local/bin
cp pixie.service /etc/systemd/system/
