.PHONY: clean dev build test fmt format release

clean:
	cargo clean

dev:
	cargo run

build:
	cargo build --release

test:
	cargo test

fmt:
	cargo fmt

format: fmt

release:
	bash scripts/release/pushReleaseTag.sh $(RELEASE_FLAGS)
