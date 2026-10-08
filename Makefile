# auto-compress-apfs: build and install (macOS only).

BIN        := auto-compress-apfs
LINK       := $(HOME)/.local/bin/$(BIN)
CONFIG     := $(HOME)/.config/$(BIN)/default.toml

# `install` must run its steps in order.
.NOTPARALLEL:
.PHONY: build release link config install test lint

build:
	cargo build

# Optimised build (lto = "fat", see Cargo.toml); exactly one artefact in release/.
release:
	cargo build --release --locked
	mkdir -p release
	install -m 0755 target/release/$(BIN) release/

link:
	mkdir -p $(HOME)/.local/bin
	ln -sfn $(CURDIR)/release/$(BIN) $(LINK)

# Copies the default config only when none exists yet.
config:
	@if [ -e "$(CONFIG)" ]; then \
		echo "$(CONFIG) exists, left as is"; \
	else \
		mkdir -p "$(dir $(CONFIG))" && cp config/default.toml "$(CONFIG)" && echo "created $(CONFIG)"; \
	fi

install: release link config

test:
	cargo test

lint:
	cargo fmt --check
	cargo clippy --all-targets -- -D warnings

# Optional local, untracked additions.
-include local.mk
