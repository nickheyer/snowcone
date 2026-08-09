.PHONY: clean dev build test fmt format

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
