.PHONY: check-vendored-mnestic

MNESTIC_TARGET_DIR ?= $(CURDIR)/target/vendor-mnestic

# mnestic is deliberately excluded from the root workspace, so root `cargo
# test` cannot exercise its fork-specific regression suite.
check-vendored-mnestic:
	CARGO_TARGET_DIR="$(MNESTIC_TARGET_DIR)" cargo test \
		--manifest-path vendor/mnestic/Cargo.toml \
		--locked \
		--offline
