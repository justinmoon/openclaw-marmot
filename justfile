set shell := ["bash", "-eu", "-o", "pipefail", "-c"]

default:
    @just --list

fmt:
    cargo fmt --check

clippy:
    cargo clippy -p marmotd -- -D warnings

test:
    cargo test -p marmotd -- --nocapture

phase1:
    ./scripts/phase1.sh

phase2:
    ./scripts/phase2.sh

phase3:
    ./scripts/phase3.sh

phase3-audio:
    ./scripts/phase3_audio.sh

pre-merge:
    just fmt
    just clippy
    just test
    just phase1
    just phase2
    just phase3
    just phase3-audio
