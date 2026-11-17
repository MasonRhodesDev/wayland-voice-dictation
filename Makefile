BINDIR  ?= $(HOME)/.local/bin
UNITDIR ?= $(HOME)/.config/systemd/user

BIN     := $(BINDIR)/voice-dictation
UNIT    := $(UNITDIR)/voice-dictation.service
RELEASE := target/release/voice-dictation

SHADOW  := $(HOME)/.cargo/bin/voice-dictation

.PHONY: all build install uninstall doctor model clean

all: build

build:
	cargo build --release

install: build
	install -Dm755 $(RELEASE) $(BIN)
	install -Dm644 packaging/systemd/voice-dictation.service $(UNIT)
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
