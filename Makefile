SHELL := /bin/zsh

.PHONY: build test release app clean

build:
	cargo build

test:
	cargo test

release:
	cargo build --release

app:
	./macos/build-app.sh

clean:
	cargo clean
	rm -rf .build/MacEvery.app .build/macevery-swift-module-cache
