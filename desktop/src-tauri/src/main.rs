//! Rhapsody desktop — the macOS app that supervises the `symphonyd` sidecar and shows its dashboard.
//! Tauri v2 port of the Wails shell (`$REF/desktop/main.go` + `app.go`). P7-D1 delivers the window
//! shell (status header + the daemon dashboard once healthy) and the compiled-in build stamp; the
//! supervisor (D2), tray + lifecycle (D3), settings/credential/onboarding (D4), and packaging (D5)
//! land in later chain tasks.

mod status;
mod version;

use status::StatusDto;
use version::VersionDto;

/// The current status snapshot for the window shell / tray. Mirrors Go `App.Status`
/// (`$REF/desktop/app.go`); until the supervisor is wired (D2) it reports the `sup == nil` snapshot.
#[tauri::command]
fn status() -> StatusDto {
    status::snapshot()
}

/// The compiled-in build stamp for the footer. Mirrors Go `App.AppVersion` (`$REF/desktop/app.go`).
#[tauri::command]
fn app_version() -> VersionDto {
    version::dto()
}

fn main() {
    // Errors are values (no panic on the startup path): mirror Go `main`, which logs the run error
    // ($REF/desktop/main.go). A failed launch exits non-zero so a supervising shell notices.
    if let Err(err) = tauri::Builder::default()
        .invoke_handler(tauri::generate_handler![status, app_version])
        .run(tauri::generate_context!())
    {
        eprintln!("Rhapsody desktop failed to start: {err}");
        std::process::exit(1);
    }
}
