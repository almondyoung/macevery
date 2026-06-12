SHELL := /bin/zsh

.PHONY: build test release app dist clean

build:
	cargo build

test:
	cargo test

release:
	cargo build --release

app:
	./macos/build-app.sh

dist: app
	mkdir -p .build/release
	COPYFILE_DISABLE=1 ditto -c -k --norsrc --keepParent .build/MacEvery.app .build/release/MacEvery-macos.zip
	shasum -a 256 .build/release/MacEvery-macos.zip > .build/release/MacEvery-macos.zip.sha256

clean:
	cargo clean
	rm -rf .build/MacEvery.app .build/macevery-swift-module-cache
