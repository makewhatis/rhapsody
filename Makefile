.PHONY: test lint fixtures app dmg _sign _dmg _notarize verify-icon

test:
	cargo test --workspace

lint:
	cargo fmt --all --check
	cargo clippy --workspace --all-targets -- -D warnings
	.github/check-forbidden-tokens.sh

# Recapture golden fixtures from the reference Go daemon (operator machine only; see harness/capture/)
fixtures:
	harness/capture/capture.sh

# --- macOS desktop app (P7-D5, desktop/) — parity port of $REF/Makefile's app/dmg/sign targets ---
# The Tauri app is its OWN cargo workspace (desktop/), so these targets `cd desktop`, mirroring the Go
# Makefile which `cd desktop` into the Wails module. Prereqs: the Tauri CLI (`cargo install
# tauri-cli --version "^2"`), Rust, Node + npm, and the Xcode command-line tools. NO code-signing /
# notarization by default — the Tauri bundler ad-hoc self-signs, which is what "unsigned" means here.

BINARY       := rhapsodyd
# Build stamp compiled into the desktop app footer (mirrors the Go Makefile `-ldflags` injection into
# $REF/desktop/internal/version, here via the RHAPSODY_* env vars src-tauri/build.rs + version.rs read).
# VERSION is the release version — bump on release: `make app VERSION=1.2.0`. Dev builds default to
# "dev" and carry the short git SHA (+ "-dirty" for an uncommitted tracked tree).
VERSION      ?= dev
COMMIT       := $(shell git rev-parse --short HEAD 2>/dev/null || echo none)
DIRTY        := $(shell test -n "$$(git status --porcelain --untracked-files=no 2>/dev/null)" && echo -dirty)
BUILDTIME    := $(shell date -u +%Y-%m-%dT%H:%M:%SZ)

APP          := desktop/target/release/bundle/macos/Rhapsody.app
DMGOUT       := desktop/build/bin/Rhapsody.dmg
DMG_VOLNAME  := Rhapsody
ENTITLEMENTS := desktop/build/darwin/entitlements.plist
APPICON      := desktop/src-tauri/icons/icon.png

# app builds the UNSIGNED macOS Rhapsody.app (Tauri bundler) with a freshly-built rhapsodyd embedded
# as the sidecar. Mirrors the Go `app` target (build-web + build-go + wails build + cp sidecar):
#   1. the daemon's embedded React dashboard (web/ -> crates/httpapi/web-dist, rust-embed source),
#   2. the Tauri shell's own frontend bundle (desktop/frontend/dist, tauri.conf.json frontendDist),
#   3. the release rhapsodyd daemon (embeds #1),
#   4. the Tauri app bundle — `--no-default-features` drops the `fakedaemon` test-stub bin so it is
#      NOT shipped in Contents/MacOS (the bundler copies every built package bin),
#   5. the sidecar copied to Contents/Resources/rhapsodyd (where supervisor/resolve.rs looks).
# Output: desktop/target/release/bundle/macos/Rhapsody.app.
app:
	cd web && npm ci && npm run build
	touch crates/httpapi/web-dist/.gitkeep
	cd desktop/frontend && npm ci && npm run build
	touch desktop/frontend/dist/.gitkeep
	cargo build --release -p $(BINARY)
	cd desktop && RHAPSODY_VERSION="$(VERSION)" RHAPSODY_COMMIT="$(COMMIT)$(DIRTY)" RHAPSODY_BUILD_TIME="$(BUILDTIME)" \
		cargo tauri build --bundles app -- --no-default-features
	mkdir -p "$(APP)/Contents/Resources"
	cp target/release/$(BINARY) "$(APP)/Contents/Resources/$(BINARY)"
	@echo "Built $(APP) (unsigned) with embedded $(BINARY) sidecar"

# dmg builds the app, (optionally) signs it, packages a drag-to-Applications Rhapsody.dmg installer,
# (optionally) notarizes + staples it, and confirms the icon flowed through. Signing and notarization
# are GATED and independent: with no creds the build is UNSIGNED; with APPLE_SIGNING_IDENTITY (+ a
# NOTARY_PROFILE) set it produces a signed (+ notarized) installer — see desktop/SIGNING.md. Prereqs
# (beyond `make app`): create-dmg (`brew install create-dmg`) for the polished installer; without it
# the target falls back to hdiutil (no extra dependency). Output: desktop/build/bin/Rhapsody.dmg.
dmg: app
	$(MAKE) _sign
	$(MAKE) _dmg
	$(MAKE) _notarize
	$(MAKE) verify-icon

# _sign code-signs $(APP) and its embedded rhapsodyd sidecar under the hardened runtime with
# entitlements (Developer ID). No-op unless APPLE_SIGNING_IDENTITY is set. See desktop/SIGNING.md.
_sign:
	bash desktop/scripts/sign.sh "$(APP)" "$(ENTITLEMENTS)"

# _dmg packages an already-built (and, if creds are set, already-signed) $(APP) into $(DMGOUT)
# (create-dmg, hdiutil fallback). Split out so it can be re-run without rebuilding the whole app.
_dmg:
	bash desktop/scripts/make-dmg.sh "$(APP)" "$(DMGOUT)" "$(DMG_VOLNAME)"

# _notarize submits $(DMGOUT) to Apple's notary service and staples the ticket. No-op unless
# NOTARY_PROFILE (or an ASC_* API key) is set (and the app inside must already be signed). See SIGNING.md.
_notarize:
	bash desktop/scripts/notarize.sh "$(DMGOUT)"

# verify-icon confirms the build consumed the source icon into $(APP)'s .icns. Runs against a built $(APP).
verify-icon:
	bash desktop/scripts/verify-icon.sh "$(APP)" "$(APPICON)"
