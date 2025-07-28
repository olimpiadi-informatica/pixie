#!/usr/bin/env bash
set -xe

SELFDIR="$(realpath "$(dirname "$0")")"
cd "$SELFDIR"

./setup.sh

trap '' SIGTERM
sudo ./run_test.sh ${SELFDIR}/storage
