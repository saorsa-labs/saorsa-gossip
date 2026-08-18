# cargo-hakari version that generated saorsa-gossip-workspace-hack.
# Keep this in sync with .config/hakari.toml, CI, and release.yml.
CARGO_HAKARI_VERSION ?= 0.9.38

.PHONY: hakari-verify hakari-generate

define require-cargo-hakari
	@v="$$(cargo hakari --version 2>/dev/null | awk '{print $$2}')"; \
	if [ "$$v" != "$(CARGO_HAKARI_VERSION)" ]; then \
	  echo "error: cargo-hakari $${v:-not installed} does not match pinned $(CARGO_HAKARI_VERSION)" >&2; \
	  echo "install: cargo install cargo-hakari --version $(CARGO_HAKARI_VERSION) --locked" >&2; \
	  exit 1; \
	fi
endef

hakari-verify:
	$(require-cargo-hakari)
	cargo hakari verify

hakari-generate:
	$(require-cargo-hakari)
	cargo hakari generate
