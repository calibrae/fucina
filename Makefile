BINARY = fucina
IDENTIFIER = net.calii.fucina
SIGN_IDENTITY = Developer ID Application: Nico Bousquet (XJQQCN392F)
ENTITLEMENTS = entitlements.plist

.PHONY: build release sign clean

build:
	cargo build

release:
	cargo build --release

sign: release
	codesign --force --options runtime \
		--sign "$(SIGN_IDENTITY)" \
		--identifier "$(IDENTIFIER)" \
		--entitlements $(ENTITLEMENTS) \
		target/release/$(BINARY)
	codesign -dvvv target/release/$(BINARY)

linux:
	cargo build --release --target x86_64-unknown-linux-gnu

clean:
	cargo clean
