# Signing & notarizing Rhapsody.app (individual Apple Developer account)

`make dmg` produces an **unsigned** `Rhapsody.dmg` by default (see [`README.md`](./README.md)). This
runbook turns on the **gated** Developer ID path so a human can produce a **signed + notarized**
installer that opens on other Macs without Gatekeeper warnings. It is the Tauri-shell parity of the
Go/Wails runbook (`$REF/desktop/SIGNING.md`).

Everything here is **opt-in via environment variables** — with none set, `make dmg` stays on the
unsigned path and never touches your keychain or the network. There are no signing secrets in the
repo or CI; this is a local, human-run step.

> **TL;DR** — once set up (steps 1–3, one time):
> ```sh
> APPLE_SIGNING_IDENTITY="Developer ID Application: Your Name (TEAMID)" \
> NOTARY_PROFILE=rhapsody-notary \
> make dmg
> ```

## What the gated build does

`make dmg` runs `app → _sign → _dmg → _notarize → verify-icon`. The two gated steps key off
independent environment variables:

| Variable | When set… | When unset… |
| --- | --- | --- |
| `APPLE_SIGNING_IDENTITY` | `_sign` code-signs the **`rhapsodyd` sidecar first, then the app bundle** under the hardened runtime (`--options runtime`), with a secure `--timestamp` and `build/darwin/entitlements.plist`. | `_sign` is a no-op; the dmg contains the Tauri ad-hoc-signed (i.e. unsigned) app. |
| `NOTARY_PROFILE` | `_notarize` submits the finished dmg to Apple (`notarytool submit --wait`), then `stapler staple`s the ticket. | `_notarize` is a no-op. |

The two gate **independently**: set only `APPLE_SIGNING_IDENTITY` to get a *signed-but-unnotarized*
dmg (useful for local testing), or both for a fully distributable installer.

Why sign the sidecar separately: `rhapsodyd` is a **distinct Mach-O** copied into
`Rhapsody.app/Contents/Resources/rhapsodyd`. Under the hardened runtime the app can't load/launch a
nested binary that isn't validly signed, so we sign it **inside-out** (sidecar, then the app — which
reseals the bundle around the freshly-signed sidecar). The `disable-library-validation` entitlement
(in `build/darwin/entitlements.plist`) lets the app load/exec code not signed with its exact
identity; see the comments in that file for why the other hardened-runtime exceptions are omitted.

## Prerequisites

- An **Apple Developer Program** membership (the individual/personal account is fine).
- **Xcode** (or at least the command-line tools), which provide `codesign`, `xcrun notarytool`, and
  `xcrun stapler` (notarytool requires Xcode 13+).
- A working `make app` toolchain (the Tauri CLI, Rust, Node) — see [`README.md`](./README.md).

## 1. Create a Developer ID Application certificate

In **Xcode ▸ Settings ▸ Accounts**, select your Apple ID ▸ **Manage Certificates…** ▸ **+** ▸
**Developer ID Application**. (Equivalently, create it on the Apple Developer portal and import it
into your login keychain.) Then confirm it's available for code signing:

```sh
security find-identity -v -p codesigning
```

You should see a line like:

```
1) ABCD1234… "Developer ID Application: Your Name (TEAMID)"
```

The quoted string is your **`APPLE_SIGNING_IDENTITY`**. Use the full `"Developer ID Application: …"`
form (not the SHA-1) so the right cert is picked even if you hold several.

## 2. Note your Team ID

The 10-character code in parentheses in that identity (`TEAMID` above) is your **Team ID**. You can
also find it at <https://developer.apple.com/account> ▸ Membership. You'll need it for the notary
profile.

## 3. Create a notarytool keychain profile (one time)

Notarization authenticates with an **app-specific password**, stored once in your keychain so it's
not passed on the command line:

1. At <https://appleid.apple.com> ▸ **Sign-In and Security ▸ App-Specific Passwords**, generate a
   password (label it e.g. `rhapsody-notary`).
2. Store the credentials under a profile name (here, `rhapsody-notary` — this becomes your
   **`NOTARY_PROFILE`**):

   ```sh
   xcrun notarytool store-credentials rhapsody-notary \
     --apple-id "you@example.com" \
     --team-id "TEAMID" \
     --password "abcd-efgh-ijkl-mnop"   # the app-specific password from step 1
   ```

`notarytool` validates and saves the profile to your keychain; you won't need the password again.

## 4. Build a signed + notarized installer

From the repo root:

```sh
APPLE_SIGNING_IDENTITY="Developer ID Application: Your Name (TEAMID)" \
NOTARY_PROFILE=rhapsody-notary \
make dmg
```

This builds the app, signs the sidecar + app, packages `desktop/build/bin/Rhapsody.dmg`, submits it
for notarization (waits for the result), and staples the ticket.

> **`create-dmg` needs a GUI session.** The polished installer (`brew install create-dmg`) drives
> Finder via AppleScript, so run it from a logged-in desktop (not a headless SSH session). Without
> `create-dmg` the build falls back to `hdiutil`, which works headless but yields a plainer layout.
> `create-dmg` is occasionally flaky; the build retries once automatically.

## 5. Verify the result

```sh
# The dmg is accepted by Gatekeeper (notarized + stapled):
spctl --assess --type open --context context:primary-signature -v desktop/build/bin/Rhapsody.dmg

# The app's signature (and its nested sidecar) is valid under the hardened runtime:
codesign --verify --deep --strict --verbose=2 desktop/target/release/bundle/macos/Rhapsody.app
codesign -dv --verbose=4 desktop/target/release/bundle/macos/Rhapsody.app   # check Authority + Identifier

# The notarization ticket is stapled (the dmg validates offline):
xcrun stapler validate desktop/build/bin/Rhapsody.dmg
```

`codesign -dv` should report `Authority=Developer ID Application: …`, `TeamIdentifier=TEAMID`, and
`Identifier=is.makewhat.rhapsody`.

## Notes

- **Identity is not hard-coded.** The signing identity comes entirely from `APPLE_SIGNING_IDENTITY`,
  so shipping under an org Developer ID later is a drop-in: install that cert, set a different
  `APPLE_SIGNING_IDENTITY` (and a notary profile for that team), and nothing else changes. The
  **bundle id stays `is.makewhat.rhapsody`** (`tauri.conf.json` `identifier`) regardless of who signs it.
- **First-open after notarization.** A correctly notarized + stapled dmg opens with no Gatekeeper
  prompt. An unsigned/un-notarized build still works for local use via right-click ▸ Open or
  `xattr -dr com.apple.quarantine Rhapsody.app`.
- **Nested-binary placement.** The sidecar lives in `Contents/Resources/` (where `make app` copies
  it, and where the supervisor's `resolve.rs` looks). We sign it explicitly before sealing the
  bundle, which satisfies `codesign --verify --deep --strict` and notarization. If a future macOS
  tightens nested-code rules, the canonical fix is to relocate it under `Contents/MacOS/` or
  `Contents/Helpers/`; that's a daemon-embedding change (out of scope here).
- **CI notarization (throwaway runners).** Keychain profiles are per-machine, so `_notarize` also
  accepts an App Store Connect API key via `ASC_KEY_ID` + `ASC_ISSUER_ID` + `ASC_API_KEY_P8` (or
  `ASC_API_KEY_P8_BASE64`, decoded to a chmod-600 temp file). A partial trio is a loud error, never a
  silent skip. Rhapsody's own CI never sets these — the signed dmg is a human-gated, local step.
- **Troubleshooting notarization.** If `notarytool submit` reports `Invalid`, fetch the detailed log
  with `xcrun notarytool log <submission-id> --keychain-profile rhapsody-notary`; the usual causes
  are a missing hardened runtime (`--options runtime`) or an unsigned nested binary — both of which
  `_sign` already handles for the app + sidecar.
