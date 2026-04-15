# Makefile: build, test, and release orchestration for myrc.
#
# Requires:
#   - Rust toolchain (rustup, cargo)
#   - zig (brew install zig)
#   - cargo-zigbuild (cargo install cargo-zigbuild)
#
# Usage:
#   make build          Build debug binary (native)
#   make build-release  Build release binary (native)
#   make test           Run tests and clippy
#   make release        Cross-compile all platforms, package, checksum
#   make install        Install to $CARGO_HOME/bin
#   make install PREFIX=<path>
#                       Full install: binary + man pages + completions
#   make clean          Remove build artifacts
#   make help           Show this help

BINARY_NAME := myrc
VERSION     := $(shell grep '^version' Cargo.toml | head -1 | sed 's/.*"\(.*\)"/\1/')
GIT_VERSION := $(shell git describe --tags --always --dirty 2>/dev/null || echo "v$(VERSION)")
DIST_DIR    := dist

# All Slurm-supporting Linux targets.
# Glibc suffix pins minimum version for RHEL 7+/8+ compatibility.
# ppc64le requires >= 2.19 (Zig lacks earlier glibc stubs for that arch).
TARGETS := \
	x86_64-unknown-linux-gnu.2.17 \
	aarch64-unknown-linux-gnu.2.17 \
	powerpc64le-unknown-linux-gnu.2.19

# Release profile flags.
RELEASE_FLAGS := --release

# ───────────────────────────────────────────────────────────────────────
# Development
# ───────────────────────────────────────────────────────────────────────

.PHONY: build build-release test clippy fmt check install clean release help

default: build

all: clean fmt check test build-release

build:
	cargo build

build-release:
	cargo build $(RELEASE_FLAGS)

# ───────────────────────────────────────────────────────────────────────
# Quality
# ───────────────────────────────────────────────────────────────────────

test:
	cargo test --all
	cargo clippy -- -D warnings

clippy:
	cargo clippy -- -D warnings

fmt:
	cargo fmt

check:
	cargo fmt --check
	cargo clippy -- -D warnings

# ───────────────────────────────────────────────────────────────────────
# Install
#
# Usage:
#   make install                  Install to $CARGO_HOME/bin (dev)
#   make install PREFIX=/sw/...   Full install: binary + man pages + completions
# ───────────────────────────────────────────────────────────────────────

PREFIX ?=

install:
ifeq ($(PREFIX),)
	cargo install --path .
else
	@echo "Installing $(BINARY_NAME) to $(PREFIX)..."
	cargo build $(RELEASE_FLAGS)
	@# Binary
	install -d $(PREFIX)/bin
	install -m 755 target/release/$(BINARY_NAME) $(PREFIX)/bin/
	@# Man pages
	@echo "Generating man pages..."
	target/release/$(BINARY_NAME) generate-man $(PREFIX)/share/man/man1
	@# Shell completions
	@echo "Generating shell completions..."
	install -d $(PREFIX)/share/bash-completion/completions
	install -d $(PREFIX)/share/zsh/site-functions
	install -d $(PREFIX)/share/fish/vendor_completions.d
	target/release/$(BINARY_NAME) completions bash > $(PREFIX)/share/bash-completion/completions/$(BINARY_NAME)
	target/release/$(BINARY_NAME) completions zsh  > $(PREFIX)/share/zsh/site-functions/_$(BINARY_NAME)
	target/release/$(BINARY_NAME) completions fish > $(PREFIX)/share/fish/vendor_completions.d/$(BINARY_NAME).fish
	@echo ""
	@echo "Installed to $(PREFIX):"
	@echo "  $(PREFIX)/bin/$(BINARY_NAME)"
	@echo "  $(PREFIX)/share/man/man1/myrc*.1"
	@echo "  $(PREFIX)/share/bash-completion/completions/$(BINARY_NAME)"
	@echo "  $(PREFIX)/share/zsh/site-functions/_$(BINARY_NAME)"
	@echo "  $(PREFIX)/share/fish/vendor_completions.d/$(BINARY_NAME).fish"
endif

# ───────────────────────────────────────────────────────────────────────
# Release: cross-compile all platforms, package tarballs, checksums
#
# Uses cargo-zigbuild with Zig as a cross-linker.
# Rustup targets are added automatically if missing.
# ───────────────────────────────────────────────────────────────────────

release: clean-dist
	@echo "═══════════════════════════════════════════════════════"
	@echo "  Building $(BINARY_NAME) $(GIT_VERSION) for release"
	@echo "═══════════════════════════════════════════════════════"
	@command -v zig >/dev/null 2>&1 || { echo "Error: zig not found. Install with: brew install zig"; exit 1; }
	@command -v cargo-zigbuild >/dev/null 2>&1 || { echo "Error: cargo-zigbuild not found. Install with: cargo install cargo-zigbuild"; exit 1; }
	@mkdir -p $(DIST_DIR)

	@for target in $(TARGETS); do \
		echo ""; \
		echo "──────────────────────────────────────────────────"; \
		echo "  Building $$target"; \
		echo "──────────────────────────────────────────────────"; \
		BASE_TARGET=$$(echo $$target | sed 's/\.[0-9]*\.[0-9]*$$//'); \
		rustup target add $$BASE_TARGET 2>/dev/null || true; \
		cargo zigbuild $(RELEASE_FLAGS) --target $$target || exit 1; \
		\
		STAGING=$(DIST_DIR)/staging_$$target; \
		mkdir -p $$STAGING; \
		cp target/$$BASE_TARGET/release/$(BINARY_NAME) $$STAGING/; \
		if [ -f LICENSE ]; then cp LICENSE $$STAGING/; fi; \
		\
		ARCHIVE=$(BINARY_NAME)_$(GIT_VERSION)_$${BASE_TARGET}.tar.gz; \
		tar -czf $(DIST_DIR)/$$ARCHIVE -C $$STAGING .; \
		echo "  → $(DIST_DIR)/$$ARCHIVE"; \
		rm -rf $$STAGING; \
	done

	@echo ""
	@echo "──────────────────────────────────────────────────────"
	@echo "  Generating checksums"
	@echo "──────────────────────────────────────────────────────"
	@cd $(DIST_DIR) && (shasum -a 256 *.tar.gz 2>/dev/null || sha256sum *.tar.gz) > checksums.txt
	@cat $(DIST_DIR)/checksums.txt

	@echo ""
	@echo "═══════════════════════════════════════════════════════"
	@echo "  Release artifacts ready in $(DIST_DIR)/"
	@echo "═══════════════════════════════════════════════════════"
	@ls -lh $(DIST_DIR)/

# Build a single target (usage: make release-target TARGET=x86_64-unknown-linux-gnu.2.17)
release-target:
	@test -n "$(TARGET)" || (echo "Usage: make release-target TARGET=<triple[.glibc]>" && exit 1)
	@echo "Building $(BINARY_NAME) $(GIT_VERSION) for $(TARGET)..."
	@command -v zig >/dev/null 2>&1 || { echo "Error: zig not found."; exit 1; }
	@command -v cargo-zigbuild >/dev/null 2>&1 || { echo "Error: cargo-zigbuild not found."; exit 1; }
	@mkdir -p $(DIST_DIR)
	$(eval BASE_TARGET := $(shell echo $(TARGET) | sed 's/\.[0-9]*\.[0-9]*$$//'))
	@rustup target add $(BASE_TARGET) 2>/dev/null || true
	cargo zigbuild $(RELEASE_FLAGS) --target $(TARGET)
	@STAGING=$(DIST_DIR)/staging_$(BASE_TARGET); \
		mkdir -p $$STAGING; \
		cp target/$(BASE_TARGET)/release/$(BINARY_NAME) $$STAGING/; \
		if [ -f LICENSE ]; then cp LICENSE $$STAGING/; fi; \
		ARCHIVE=$(BINARY_NAME)_$(GIT_VERSION)_$(BASE_TARGET).tar.gz; \
		tar -czf $(DIST_DIR)/$$ARCHIVE -C $$STAGING .; \
		echo "→ $(DIST_DIR)/$$ARCHIVE"; \
		rm -rf $$STAGING

# ───────────────────────────────────────────────────────────────────────
# Clean
# ───────────────────────────────────────────────────────────────────────

clean:
	cargo clean
	rm -rf $(DIST_DIR)

clean-dist:
	rm -rf $(DIST_DIR)

# ───────────────────────────────────────────────────────────────────────
# Help
# ───────────────────────────────────────────────────────────────────────

help:
	@echo "$(BINARY_NAME) build system (version: $(GIT_VERSION))"
	@echo ""
	@echo "Development:"
	@echo "  make build          Build debug binary (native)"
	@echo "  make build-release  Build release binary (native)"
	@echo "  make test           Run tests + clippy"
	@echo "  make clippy         Run clippy only"
	@echo "  make fmt            Format code"
	@echo "  make check          Verify formatting + clippy (CI-style)"
	@echo "  make install        Install to cargo bin directory"
	@echo "  make install PREFIX=<path>"
	@echo "                      Full install: binary + man pages + completions"
	@echo ""
	@echo "Release:"
	@echo "  make release        Cross-compile all platforms, tarball, checksum"
	@echo "  make release-target TARGET=<triple>"
	@echo "                      Build + package a single target"
	@echo ""
	@echo "Targets: $(TARGETS)"
	@echo ""
	@echo "Prerequisites:"
	@echo "  brew install zig"
	@echo "  cargo install cargo-zigbuild"
	@echo ""
	@echo "Clean:"
	@echo "  make clean          Remove target/ and dist/"
	@echo "  make clean-dist     Remove dist/ only"
