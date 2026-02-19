BINARY := irc-log-viewer
ROUTER := root@glrouter
REMOTE_DIR := /usr/local/bin

CROSS_TARGET := aarch64-unknown-linux-musl
CROSS_LINKER := aarch64-linux-gnu-gcc

.PHONY: build release cross deploy clean check test

build:
	cargo build

release:
	cargo build --release

check:
	cargo check --message-format=short
	cargo clippy --fix --allow-dirty --message-format=short
	cargo test

test:
	cargo test

cross:
	CARGO_TARGET_AARCH64_UNKNOWN_LINUX_MUSL_LINKER=$(CROSS_LINKER) \
		cargo build --release --target $(CROSS_TARGET)

deploy: cross
	scp target/$(CROSS_TARGET)/release/$(BINARY) $(ROUTER):$(REMOTE_DIR)/$(BINARY)

clean:
	cargo clean
