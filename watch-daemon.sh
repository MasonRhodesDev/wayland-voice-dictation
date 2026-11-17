#!/bin/bash

RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
NC='\033[0m'

echo -e "${GREEN}Voice Dictation Daemon Watcher${NC}"
echo -e "${YELLOW}Watching for changes in dictation-engine and slint-gui...${NC}"
echo ""

pkill -TERM -f "voice-dictation daemon" 2>/dev/null

run_daemon() {
    echo -e "${GREEN}[$(date +%H:%M:%S)] Building and starting daemon...${NC}"

    pkill -TERM -f "voice-dictation daemon" 2>/dev/null
    sleep 0.5

    cargo build 2>&1 | grep -E "(Compiling|Finished|error|warning:.*error)" || true

    if [ ${PIPESTATUS[0]} -eq 0 ]; then
        echo -e "${GREEN}[$(date +%H:%M:%S)] Build successful, starting daemon...${NC}"

        RUST_LOG=info GUI_LOG=info ./target/debug/voice-dictation daemon 2>&1 | \
            grep -v "ALSA lib" | \
            sed "s/^/${GREEN}[DAEMON]${NC} /"
    else
        echo -e "${RED}[$(date +%H:%M:%S)] Build failed${NC}"
    fi
}

if ! command -v cargo-watch &> /dev/null; then
    echo -e "${YELLOW}cargo-watch not found. Installing...${NC}"
    cargo install cargo-watch
fi

cargo watch \
    -w dictation-engine/src \
    -w slint-gui/src \
    -w dictation-types/src \
    -w src \
    -x 'build' \
    -s 'pkill -TERM -f "voice-dictation daemon" 2>/dev/null || true' \
    -d 1
