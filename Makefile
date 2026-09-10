.DEFAULT_GOAL := build

CARGO ?= cargo
CARGO_TARGET_DIR ?= target
PREFIX ?= $(HOME)/.local
BINDIR ?= $(PREFIX)/bin
DESTDIR ?=

.PHONY: build install

build:
	$(CARGO) build --release --locked --bins --target-dir "$(CARGO_TARGET_DIR)"

install: build
	install -d "$(DESTDIR)$(BINDIR)"
	@set -eu; \
	temporary=; \
	trap 'if [ -n "$$temporary" ]; then rm -f "$$temporary"; fi' 0; \
	for binary in tarsier tarsier-mcp; do \
		temporary=$$(mktemp "$(DESTDIR)$(BINDIR)/.$$binary.XXXXXX"); \
		install -m 755 "$(CARGO_TARGET_DIR)/release/$$binary" "$$temporary"; \
		mv -f "$$temporary" "$(DESTDIR)$(BINDIR)/$$binary"; \
		temporary=; \
		printf 'Installed %s\n' "$(DESTDIR)$(BINDIR)/$$binary"; \
	done
