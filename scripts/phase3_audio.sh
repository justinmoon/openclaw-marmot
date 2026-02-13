#!/usr/bin/env bash
set -Eeuo pipefail

cd "$(dirname "${BASH_SOURCE[0]}")/.."

FRAMES="${FRAMES:-50}"

cargo run -p marmotd -- scenario audio-echo --frames "${FRAMES}"
