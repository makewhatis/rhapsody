//! obslog — orchestrator-internal port of Go `internal/obslog` (per-ticket agent transcripts).
//!
//! Go's package has no dedicated Rust crate, so it lives here. O1 ports only the *path* surface the
//! effective view and the API snapshot need — [`Store::new`] + [`Store::latest_path`]. The run
//! transcript *writer* (Go `Store.Open` / `Run`, which creates timestamped `*.jsonl` files and
//! repoints `latest.jsonl`) lands with the worker (O3), which is the only thing that opens
//! transcripts.

use std::path::Path;

use rhapsody_workspace::sanitize_key;

/// Roots per-ticket transcripts under a logs directory. Mirrors Go `obslog.Store`.
pub struct Store {
    dir: String,
}

impl Store {
    /// Returns a `Store` writing under `dir`. Mirrors Go `obslog.NewStore`.
    pub fn new(dir: impl Into<String>) -> Store {
        Store { dir: dir.into() }
    }

    /// The stable path to a ticket's most recent transcript: `<dir>/<sanitized>/latest.jsonl`.
    /// Mirrors Go `Store.LatestPath`.
    ///
    /// Go derives the dir-safe token with a private `sanitize` that, per its own doc, "mirrors
    /// `workspace.SanitizeKey`" (replace every char outside `[A-Za-z0-9._-]` with `_`, then map the
    /// traversal-unsafe results `""`, `"."`, `".."` to `"_"`). This reuses the committed
    /// [`rhapsody_workspace::sanitize_key`] rather than duplicating that logic, so the two stay in
    /// lockstep.
    pub fn latest_path(&self, ticket: &str) -> String {
        Path::new(&self.dir)
            .join(sanitize_key(ticket))
            .join("latest.jsonl")
            .to_string_lossy()
            .into_owned()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Mirrors Go `obslog` `TestLatestPath` + `TestSanitizeTicket`: the stable path is
    // `<dir>/<sanitized>/latest.jsonl`, and separators / spaces in the ticket collapse to `_`.
    #[test]
    fn latest_path_shape_and_sanitizes_ticket() {
        let s = Store::new("/logs");
        assert_eq!(s.latest_path("MT-1"), "/logs/MT-1/latest.jsonl");
        // Separator AND space become `_` (Go `sanitize("team/MT 9") == "team_MT_9"`).
        assert_eq!(s.latest_path("team/MT 9"), "/logs/team_MT_9/latest.jsonl");
    }

    // Mirrors Go `TestSanitizeRejectsTraversalSegments`: dot segments and the empty string collapse
    // to a single safe component, and a traversal attempt can never escape the ticket tree.
    #[test]
    fn latest_path_rejects_traversal_segments() {
        let s = Store::new("/logs");
        for bad in ["", ".", ".."] {
            assert_eq!(s.latest_path(bad), "/logs/_/latest.jsonl", "ticket {bad:?}");
        }
        // A traversal attempt is reduced to a single safe path component: no separators survive, so
        // the joined path stays within the ticket tree.
        let joined = s.latest_path("../../etc");
        assert!(joined.starts_with("/logs/"), "escaped tree: {joined}");
        let ticket = joined
            .strip_prefix("/logs/")
            .and_then(|r| r.strip_suffix("/latest.jsonl"))
            .expect("shape <dir>/<ticket>/latest.jsonl");
        assert!(
            !ticket.contains('/'),
            "ticket must be one component: {ticket:?}"
        );
        assert!(
            ticket != "." && ticket != "..",
            "ticket must be safe: {ticket:?}"
        );
    }
}
