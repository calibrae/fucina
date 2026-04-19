BINARY = fucina
# Bumped from net.calii.fucina → net.calii.fucina.app so macOS sees a fresh
# bundle identity and reopens the Local Network Privacy decision path (Apple
# docs: grants can't be reset for an existing bundle ID).
IDENTIFIER = net.calii.fucina.app
PKG_IDENTIFIER = net.calii.fucina
VERSION = $(shell grep '^version' Cargo.toml | head -1 | cut -d'"' -f2)
APP_SIGN = Developer ID Application: Nico Bousquet (XJQQCN392F)
PKG_SIGN = Developer ID Installer: Nico Bousquet (XJQQCN392F)
NOTARY_PROFILE = FUCINA_NOTARY
ENTITLEMENTS = entitlements.plist
PKG_ROOT = target/pkg-root
PKG = target/$(BINARY)-$(VERSION).pkg
APP_BUNDLE = target/Fucina.app

.PHONY: build release sign bundle notarize pkg pkg-sign pkg-notarize pkg-staple dist clean

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

# Build Fucina.app bundle wrapping the CLI binary.
# The bundle is what LaunchServices registers; the embedded Info.plist
# (with NSLocalNetworkUsageDescription) is what TCC keys Local Network
# grants against on SIP-enabled macOS.
bundle: release
	rm -rf $(APP_BUNDLE)
	mkdir -p $(APP_BUNDLE)/Contents/MacOS $(APP_BUNDLE)/Contents/Resources
	cp target/release/$(BINARY) $(APP_BUNDLE)/Contents/MacOS/$(BINARY)
	chmod 755 $(APP_BUNDLE)/Contents/MacOS/$(BINARY)
	cp bundle/Fucina.icns $(APP_BUNDLE)/Contents/Resources/Fucina.icns
	sed "s/__VERSION__/$(VERSION)/g" bundle/Info.plist.template > $(APP_BUNDLE)/Contents/Info.plist
	codesign --force --options runtime --timestamp \
		--sign "$(APP_SIGN)" \
		--identifier "$(IDENTIFIER)" \
		--entitlements $(ENTITLEMENTS) \
		$(APP_BUNDLE)/Contents/MacOS/$(BINARY)
	codesign --force --options runtime --timestamp \
		--sign "$(APP_SIGN)" \
		--identifier "$(IDENTIFIER)" \
		--entitlements $(ENTITLEMENTS) \
		$(APP_BUNDLE)
	codesign -dvvv $(APP_BUNDLE)

notarize: sign
	cd target/release && zip -q $(BINARY).zip $(BINARY)
	xcrun notarytool submit target/release/$(BINARY).zip \
		--keychain-profile $(NOTARY_PROFILE) --wait

# Build signed + notarized + stapled .pkg that installs Fucina.app to /Applications
# plus a /usr/local/bin/fucina symlink.
pkg: bundle
	rm -rf $(PKG_ROOT)
	mkdir -p $(PKG_ROOT)/Applications $(PKG_ROOT)/usr/local/bin
	cp -R $(APP_BUNDLE) $(PKG_ROOT)/Applications/Fucina.app
	ln -sf /Applications/Fucina.app/Contents/MacOS/$(BINARY) $(PKG_ROOT)/usr/local/bin/$(BINARY)
	pkgbuild --root $(PKG_ROOT) \
		--identifier $(PKG_IDENTIFIER) \
		--version $(VERSION) \
		--install-location / \
		--scripts bundle/scripts \
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
	rm -rf $(PKG_ROOT) $(APP_BUNDLE) target/*.pkg target/release/$(BINARY).zip
