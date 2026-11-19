UNAME   := $(shell uname -s)
BINDIR  ?= $(HOME)/.local/bin

BIN     := $(BINDIR)/voice-dictation
RELEASE := target/release/voice-dictation

SHADOW  := $(HOME)/.cargo/bin/voice-dictation

.PHONY: all build install uninstall doctor model clean

all: build

build:
	cargo build --release

ifeq ($(UNAME),Darwin)
AGENTDIR ?= $(HOME)/Library/LaunchAgents
LOGFILE  ?= $(HOME)/Library/Logs/voice-dictation.log
LABEL    := com.voicedictation.daemon
AGENT    := $(AGENTDIR)/$(LABEL).plist

install: build
	@if [ -z "$(ORT_DYLIB_PATH)" ]; then \
		echo ">> ORT_DYLIB_PATH is not set. Point it at libonnxruntime.dylib:"; \
		echo ">>   ORT_DYLIB_PATH=/path/to/libonnxruntime.dylib make install"; \
		exit 1; \
	fi
	install -d $(BINDIR) $(AGENTDIR) $(dir $(LOGFILE))
	install -m755 $(RELEASE) $(BIN)
	sed -e 's|@BINARY@|$(BIN)|' -e 's|@LOGFILE@|$(LOGFILE)|' \
		-e 's|@ORT_DYLIB_PATH@|$(ORT_DYLIB_PATH)|' dist/macos/$(LABEL).plist > $(AGENT)
	-launchctl bootout gui/$$(id -u)/$(LABEL) 2>/dev/null
	launchctl bootstrap gui/$$(id -u) $(AGENT)
	@echo ">> Installed $(BIN); daemon log: $(LOGFILE)"
	@echo ">> Allow $(BIN) under System Settings > Privacy & Security > Microphone and Accessibility"
	@$(MAKE) --no-print-directory doctor

uninstall:
	-launchctl bootout gui/$$(id -u)/$(LABEL)
	rm -f $(BIN) $(AGENT)
	@echo ">> Uninstalled. Model & config under ~/.config/voice-dictation were kept."
else
UNITDIR ?= $(HOME)/.config/systemd/user
UNIT    := $(UNITDIR)/voice-dictation.service

install: build
	install -Dm755 $(RELEASE) $(BIN)
	install -Dm644 dist/voice-dictation.service $(UNIT)
	systemctl --user daemon-reload
	@if [ -e "$(SHADOW)" ] && [ "$(SHADOW)" != "$(BIN)" ]; then \
		echo ">> Removing stale shadow binary: $(SHADOW)"; \
		rm -f "$(SHADOW)"; \
	fi
	@if systemctl --user is-active --quiet voice-dictation; then \
		echo ">> Restarting daemon to match the installed binary"; \
		systemctl --user restart voice-dictation; \
	else \
		echo ">> Daemon not running. Start it with: systemctl --user enable --now voice-dictation"; \
	fi
	@echo ">> Installed $(BIN)"
	@$(MAKE) --no-print-directory doctor

uninstall:
	-systemctl --user disable --now voice-dictation
	rm -f $(BIN)
	rm -f $(UNIT)
	systemctl --user daemon-reload
	@echo ">> Uninstalled. Model & config under ~/.config/voice-dictation were kept."
endif

doctor:
	@echo "Binary consistency check:"
	@found=$$(printf '%s\n' "$$PATH" | tr ':' '\n' | while read -r d; do \
		[ -n "$$d" ] && [ -x "$$d/voice-dictation" ] && readlink -f "$$d/voice-dictation"; \
	done | awk '!seen[$$0]++'); \
	count=$$(printf '%s\n' "$$found" | grep -c . || true); \
	if [ "$$count" -gt 1 ]; then \
		echo "  WARNING: $$count 'voice-dictation' binaries on PATH (the FIRST wins):"; \
		printf '%s\n' "$$found" | sed 's/^/    /'; \
		echo "  Keep only $(BIN); remove the others."; \
	elif [ "$$count" -eq 1 ]; then \
		echo "  OK: single binary -> $$found"; \
	else \
		echo "  No voice-dictation found on PATH. Is $(BINDIR) on your PATH?"; \
	fi

model:
	$(BIN) download-model

clean:
	cargo clean
