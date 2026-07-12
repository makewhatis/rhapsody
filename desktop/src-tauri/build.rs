fn main() {
    // Build-time version stamp (mirrors the Go Makefile `-ldflags` injection into
    // $REF/desktop/internal/version): the Makefile `app` target (P7-D5) sets these env vars and
    // `option_env!` in src/version.rs reads them. Mark them rerun-if-env-changed so a re-stamp with
    // a fresh version/commit/build-time forces a recompile of the values.
    for key in ["RHAPSODY_VERSION", "RHAPSODY_COMMIT", "RHAPSODY_BUILD_TIME"] {
        println!("cargo:rerun-if-env-changed={key}");
    }
    tauri_build::build();
}
