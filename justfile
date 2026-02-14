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

phase8-voice:
    MARMOT_TTS_FIXTURE=1 cargo test -p marmotd daemon::tests::tts_pcm_publish_reaches_subscriber -- --nocapture

pre-merge:
    just fmt
    just clippy
    just test
    just phase1
    just phase2
    just phase3
    just phase3-audio
    just phase8-voice
