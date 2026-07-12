# Desktop build assets (macOS packaging)

Assets consumed by the P7-D5 packaging targets (`make app` / `make dmg`, root Makefile) and the
`desktop/scripts` helpers. Parity port of the Go/Wails `$REF/desktop/build` layout, trimmed to what
the Tauri bundler needs (Tauri generates `Info.plist` from `src-tauri/tauri.conf.json`, so the Wails
`Info.plist` / `appicon.png` are not needed here).

```
build/
├── darwin/
│   └── entitlements.plist   # hardened-runtime entitlements applied by scripts/sign.sh (Developer ID)
└── bin/                     # dmg output (make dmg) — git-ignored build artifact, not committed
```

- **`darwin/entitlements.plist`** — the minimal hardened-runtime entitlements
  (`com.apple.security.cs.disable-library-validation`) the gated signing path applies to the app so
  it can load the separately-signed `rhapsodyd` sidecar. See [`../SIGNING.md`](../SIGNING.md) and the
  comments in the file. Validated by the packaging gate test (`src-tauri/tests/packaging_gate.rs`).
- **`bin/`** — where `make dmg` writes `Rhapsody.dmg`. Build output (git-ignored). The unsigned app
  bundle itself lands under `desktop/target/release/bundle/macos/Rhapsody.app` (the Tauri bundler's
  output tree).

The app icon lives with the Tauri config at `src-tauri/icons/` (`icon.icns` + the png set, generated
by `cargo tauri icon`); `make dmg` runs `scripts/verify-icon.sh` to confirm it flowed into the bundle.
