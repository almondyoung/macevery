SHELL := /bin/zsh

.PHONY: build test test-gui release app notarize dist clean

build:
	cargo build

test:
	cargo test
	$(MAKE) test-gui

test-gui:
	mkdir -p .build/gui-tests/module-cache
	swiftc -O -parse-as-library -D MACEVERY_TESTING \
		-module-cache-path .build/gui-tests/module-cache \
		-target $(shell uname -m)-apple-macos13.0 \
		-o .build/gui-tests/gui-regression-checks \
		macos/MacEveryApp.swift tests/macos/GUIRegressionChecks.swift
	.build/gui-tests/gui-regression-checks

release:
	cargo build --release

app:
	./macos/build-app.sh

notarize: app
	./macos/notarize-app.sh .build/MacEvery.app

dist: app
	mkdir -p .build/release
	COPYFILE_DISABLE=1 ditto -c -k --norsrc --keepParent .build/MacEvery.app .build/release/MacEvery-macos.zip
	cd .build/release && shasum -a 256 MacEvery-macos.zip > MacEvery-macos.zip.sha256

clean:
	cargo clean
	rm -rf .build/MacEvery.app .build/macevery-swift-module-cache .build/gui-tests
