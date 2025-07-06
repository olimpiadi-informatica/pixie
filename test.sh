#!/usr/bin/env bash
set -xe

# TODO: less invasive on host system

SELFDIR="$(realpath "$(dirname "$0")")"
cd "$SELFDIR"

./setup.sh

setsid sudo ./run_test.sh ${SELFDIR}/storage
