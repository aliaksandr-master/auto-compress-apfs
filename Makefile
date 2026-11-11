# auto-compress-apfs: build and install (macOS only).

BIN        := auto-compress-apfs
LINK       := $(HOME)/.local/bin/$(BIN)
CONFIG     := $(HOME)/.config/$(BIN)/default.toml
# The copy the symlink points to: a new `make release` does not change what is installed.
INSTALLED  := $(CURDIR)/release/installed/$(BIN)

# `install` must run its steps in order; it installs the build `make release` left in release/.
.NOTPARALLEL:
.PHONY: build release link config install test lint

build:
	cargo build

# Optimised build ([profile.dist], which inherits [profile.release]: lto = "fat", see Cargo.toml); exactly one artefact, release/$(BIN).
release:
	cargo build --profile dist --locked
	mkdir -p release
	install -m 0755 target/dist/$(BIN) release/

# Installs the last `make release` build: a copy under release/installed/ plus the symlink.
link:
	@test -x release/$(BIN) || { echo "release/$(BIN) missing: run 'make release' first" >&2; exit 1; }
	mkdir -p release/installed $(HOME)/.local/bin
	install -m 0755 release/$(BIN) $(INSTALLED)
	ln -sfn $(INSTALLED) $(LINK)

# Copies the default config only when none exists yet.
config:
	@if [ -e "$(CONFIG)" ]; then \
		echo "$(CONFIG) exists, left as is"; \
	else \
		mkdir -p "$(dir $(CONFIG))" && cp config/default.toml "$(CONFIG)" && echo "created $(CONFIG)"; \
	fi

install: link config

test:
	cargo test

lint:
	cargo fmt --check
	cargo clippy --locked --all-targets -- -D warnings
	RUSTDOCFLAGS="-D warnings" cargo doc --locked --no-deps --document-private-items
	cargo deny --locked check
