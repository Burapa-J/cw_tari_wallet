# Variables
CODEGEN_TOOL := flutter_rust_bridge_codegen
DART_MOCK_DIR := .dart
RUST_GEN_FILE := src/frb_generated.rs
# Optional target triple for the release targets, e.g. TARGET=aarch64-linux-android
TARGET ?=
TARGET_FLAG := $(if $(TARGET),--target $(TARGET),)

.PHONY: help gen watch install clean lint fmt fmt-check check record-fixtures verify-live release-android release-ios release-macos release-linux release-windows

# Default target: Show help
help:
	@echo "Usage: make [target]"
	@echo ""
	@echo "Targets:"
	@echo "  gen        Generate the Rust/Dart bridge code"
	@echo "  watch      Watch for file changes and auto-generate"
	@echo "  install    Install/Update the codegen tool via Cargo"
	@echo "  clean      Remove all generated bridge files"
	@echo "  fmt        Format all code with rustfmt"
	@echo "  fmt-check  Check formatting without modifying files"
	@echo "  lint       Run clippy with warnings denied"
	@echo "  check      Type-check all targets"
	@echo "  record-fixtures  Rebuild the verification fixtures (Tier A DB + Tier B RPC)"
	@echo "  verify-live      Run the Tier C live-testnet smoke runner (opt-in, needs env secrets)"
	@echo "  release-android  Build the release cdylib with LTO (optional TARGET=<triple>)"
	@echo "  release-ios      Build the release staticlib with LTO (optional TARGET=<triple>)"
	@echo "  release-macos / release-linux / release-windows  Same as release-android (cdylib)"

gen:
	$(CODEGEN_TOOL) generate

watch:
	$(CODEGEN_TOOL) generate --watch

install:
	cargo install flutter_rust_bridge_codegen

clean:
	@echo "Cleaning generated files..."
	rm -f $(RUST_GEN_FILE)
	rm -f $(DART_MOCK_DIR)/frb_generated.dart
	rm -f $(DART_MOCK_DIR)/frb_generated.io.dart
	rm -f $(DART_MOCK_DIR)/frb_generated.web.dart
	rm -f $(DART_MOCK_DIR)/lib.dart
	rm -rf $(DART_MOCK_DIR)/api

fmt:
	cargo fmt --all

fmt-check:
	cargo fmt --all -- --check

lint:
	# `--workspace` to match CI (.github/workflows/ci.yml) so the `verify` harness
	# crate is linted here too — without it this target is strictly weaker than the
	# merge gate and lint failures in `verify/` only surface on the PR.
	cargo clippy --workspace --all-targets --all-features -- -D warnings

check:
	cargo check --all-targets

# Verification harness.
#
# Rebuild the committed verification fixtures: the Tier A view-only wallet DB and
# the Tier B recorded RPC JSON captures. Run this when the fixture's golden values
# or the upstream RPC shape changes, then commit the regenerated assets.
record-fixtures:
	cargo run -p verify -- record-fixtures

# Run the Tier C live-testnet smoke runner. Opt-in and gated: it needs a live
# esmeralda base node and a dedicated, minimally-funded test wallet supplied via
# env vars (VERIFY_BASE_URL, VERIFY_SEED_WORDS, VERIFY_PASSPHRASE). NEVER run on
# PRs — this is for local/manual or the nightly schedule only.
verify-live:
	cargo run -p verify --features live-e2e -- live

# Release artifacts. Cargo skips LTO when `crate-type` also includes `lib` (rlib),
# so build one crate type at a time to get the `[profile.release]` LTO.
# `--locked`: a release must build exactly what Cargo.lock records.
# Mainnet network group by default (tari_common fixes it at compile time; see
# CONTRIBUTING.md); pass TARI_TARGET_NETWORK=testnet for an Esmeralda build.
release-android release-macos release-linux release-windows release-ios: export TARI_TARGET_NETWORK ?= mainnet
release-android release-macos release-linux release-windows:
	cargo rustc --release --locked --lib --crate-type cdylib $(TARGET_FLAG)

release-ios:
	cargo rustc --release --locked --lib --crate-type staticlib $(TARGET_FLAG)
