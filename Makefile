# wx: build, test, lint and install, a cheap `smoke`, and the `verify` gate CI runs.

BIN := wx
CARGO := cargo

.PHONY: all build release test lint fmt fmt-check clean install deps smoke verify dev

all: verify

build: SIGN_TARGET = target/debug/$(BIN)
build:
	$(CARGO) build
	@$(SIGN)

release: SIGN_TARGET = target/release/$(BIN)
release:
	$(CARGO) build --release --locked
	@$(SIGN)

test:
	$(CARGO) test --all --locked

lint:
	$(CARGO) clippy --all-targets --locked -- -D warnings
	$(CARGO) clippy --lib --no-default-features --locked -- -D warnings

fmt:
	$(CARGO) fmt --all

fmt-check:
	$(CARGO) fmt --all -- --check

clean:
	$(CARGO) clean

# --locked installs from the reviewed Cargo.lock. The re-sign keeps one stable
# code identity across reinstalls.
install: SIGN_TARGET = $${CARGO_INSTALL_ROOT:-$$HOME/.cargo}/bin/$(BIN)
install:
	$(CARGO) install --path . --force --locked
	@$(SIGN)

deps:
	$(CARGO) fetch

dev: SIGN_TARGET = target/debug/$(BIN)
dev:
	$(CARGO) build
	@$(SIGN)

define SIGN
if [ -x "$$HOME/Dev/cli-common/scripts/dev-sign.sh" ]; then \
	"$$HOME/Dev/cli-common/scripts/dev-sign.sh" "$(SIGN_TARGET)"; \
else echo "cli-common/scripts/dev-sign.sh not found: $(SIGN_TARGET) left ad-hoc signed"; fi
endef

# No network: version, help and discovery only.
smoke: release
	./target/release/$(BIN) --version
	./target/release/$(BIN) --help > /dev/null
	./target/release/$(BIN) info > /dev/null
	./target/release/$(BIN) mcp --list-tools > /dev/null

verify: fmt-check lint test smoke
