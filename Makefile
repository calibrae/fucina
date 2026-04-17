BINARY = fucina
IDENTIFIER = net.calii.fucina
VERSION = $(shell grep '^version' Cargo.toml | head -1 | cut -d'"' -f2)
APP_SIGN = Developer ID Application: Nico Bousquet (XJQQCN392F)
PKG_SIGN = Developer ID Installer: Nico Bousquet (XJQQCN392F)
NOTARY_PROFILE = FUCINA_NOTARY
ENTITLEMENTS = entitlements.plist
PKG_ROOT = target/pkg-root
PKG = target/$(BINARY)-$(VERSION).pkg

.PHONY: build release sign notarize pkg pkg-sign pkg-notarize pkg-staple dist clean

build:
	cargo build

release:
	cargo build --release

sign: release
	codesign --force --options runtime --timestamp \
		--sign "$(APP_SIGN)" \
		--identifier "$(IDENTIFIER)" \
		--entitlements $(ENTITLEMENTS) \
		target/release/$(BINARY)
	codesign -dvvv target/release/$(BINARY)

notarize: sign
	cd target/release && zip -q $(BINARY).zip $(BINARY)
	xcrun notarytool submit target/release/$(BINARY).zip \
		--keychain-profile $(NOTARY_PROFILE) --wait

# Build signed + notarized + stapled .pkg for distribution on SIP-enabled hosts
pkg: sign
	rm -rf $(PKG_ROOT)
	mkdir -p $(PKG_ROOT)/usr/local/bin
	cp target/release/$(BINARY) $(PKG_ROOT)/usr/local/bin/$(BINARY)
	chmod 755 $(PKG_ROOT)/usr/local/bin/$(BINARY)
	pkgbuild --root $(PKG_ROOT) \
		--identifier $(IDENTIFIER) \
		--version $(VERSION) \
		--install-location / \
		--sign "$(PKG_SIGN)" \
		$(PKG)
	xcrun notarytool submit $(PKG) \
		--keychain-profile $(NOTARY_PROFILE) --wait
	xcrun stapler staple $(PKG)
	xcrun stapler validate $(PKG)

dist: pkg
	@echo "Distribution pkg: $(PKG)"
	@shasum -a 256 $(PKG)

linux:
	cargo build --release --target x86_64-unknown-linux-gnu

clean:
	cargo clean
	rm -rf $(PKG_ROOT) target/*.pkg target/release/$(BINARY).zip
