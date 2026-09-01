//! Rhapsody Teams **memory** — the pluggable [`MemoryBackend`] and its two
//! shipped implementations, `none` and `local` (STUDIO-645, slice T4; design
//! record `~/.rhapsody/docs/STUDIO-572-rhapsody-teams.md`, §5).
//!
//! §5's trait is deliberately small, because what makes memory good is the
//! *policy above the store* — STUDIO-569's retention rules and re-grounding at
//! recall — and that policy is identical for every backend. Only storage and
//! lookup differ, so only storage and lookup sit behind the trait.
//!
//! # The one rule the whole slice is built around
//!
//! **Nothing that can reach the network may sit on the dispatch path.** The
//! adversarial design review (`~/.rhapsody/docs/STUDIO-572-design-review.md`)
//! recorded that as the STUDIO-551/BO-59 head-of-line class: `dispatch_issue`
//! runs inline on the single control task, so an `await` on a remote bank there
//! would stall stop/resume/run-exit for every project at once.
//!
//! Two constructions enforce it, and both are readable from a signature:
//!
//! 1. **[`MemoryBackend`] is `async`.** T8's `hindsight` backend does HTTP, so
//!    every method must be awaitable. That is precisely why the dispatch path
//!    may never hold a `dyn MemoryBackend`: `dispatch_issue` is `fn`, not
//!    `async fn`, and cannot await one. The trait is the **off-loop** surface —
//!    the daemon's retain / recall / invalidate / reinstate endpoints, which run
//!    on the HTTP task.
//! 2. **[`LocalBank`]'s own methods are plain `fn` over local files.** That is
//!    what the dispatch path holds — concretely, never as a trait object — so a
//!    reviewer can clear the turn-1 prompt path by reading one type name. A
//!    remote backend is *unrepresentable* there, which is also how recall stays
//!    prefetchable off the dispatch path when T8 lands: `hindsight` recall will
//!    fill the same plain-data [`Fact`] slot from an off-loop step, and the
//!    renderer that consumes it never learns where the facts came from.
//!
//! # Never create anything on read (the T1/T2 rule)
//!
//! [`Teams::load`](crate::teams::Teams::load) never seeds `teams.yaml` and
//! [`profiles::resolve`](crate::profiles::resolve) never creates the profiles
//! directory. Memory keeps the rule: **the banks directory appears on the first
//! `retain` and at no other time.** Constructing the backend creates nothing,
//! and a recall against a bank that was never written returns no facts and
//! leaves the filesystem untouched.
//!
//! # Every recalled byte is turn-1 prompt cost, forever (§0.5)
//!
//! A fact recalled into the turn-1 prompt is paid on **every** run of that
//! identity for as long as it stays valid. The bounds here are therefore hard,
//! not advisory: [`Query::top_k`] caps how many records come back,
//! [`MAX_FACT_CONTENT_BYTES`] caps each one, and [`MAX_RETAIN_CONTENT_BYTES`]
//! caps what may be stored in the first place.

use std::path::{Path, PathBuf};

use async_trait::async_trait;
use chrono::{DateTime, SecondsFormat, Utc};
use serde::{Deserialize, Serialize};

/// Where `local` banks live under Rhapsody's runtime home when `memory.path` is
/// empty (§5.4: `~/.rhapsody/teams/banks/<name>/`). Relative to the same runtime
/// home `teams.yaml` and `teams/profiles/` resolve against.
pub const DEFAULT_BANKS_SUBDIR: &str = "teams/banks";

/// The file extension of one stored record. A record is a markdown document a
/// human can open — that is the whole point of `local` (§5.4).
pub const RECORD_EXT: &str = "md";

/// `state: valid` — the only state [`LocalBank::recall`] will return unless the
/// caller asks for more ([`RecallState`]). Mirrors Hindsight's
/// `readableByModel`, which "refuses **any** non-`valid` state" (§5.3), so
/// switching backends does not change what the model can see.
pub const STATE_VALID: &str = "valid";

/// `state: invalidated` — §5.3's per-record correction. The record is NOT
/// deleted: its content and the invalidation reason stay on disk, and flipping
/// the flag back restores it ([`LocalBank::set_state`]).
pub const STATE_INVALIDATED: &str = "invalidated";

/// Which record states a recall may return (STUDIO-689).
///
/// [`Valid`](RecallState::Valid) is the default everywhere and is what the
/// dispatch path asks for, so **turn-1 recall is unchanged**: a model still
/// never sees a corrected fact, exactly as §5.3's `readableByModel` requires.
/// The other two exist for the operator UI, which cannot show that a correction
/// happened — nor offer to undo one made in an earlier session — if the only
/// readable state is `valid`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum RecallState {
    /// Only `state: valid`. The default, and the only state an agent sees.
    #[default]
    Valid,
    /// Only `state: invalidated` — the corrections, and nothing else.
    Invalidated,
    /// Both, so one read can render a bank the way it is on disk.
    All,
}

impl RecallState {
    /// The wire spelling, as `GET /api/v1/teams/recall?state=` carries it.
    pub fn as_str(&self) -> &'static str {
        match self {
            RecallState::Valid => "valid",
            RecallState::Invalidated => "invalidated",
            RecallState::All => "all",
        }
    }

    /// Parses the wire spelling. Absent or empty ⇒ [`Valid`](RecallState::Valid)
    /// — the default has to survive a caller that says nothing, because every
    /// caller that predates this parameter says nothing.
    ///
    /// An unrecognised value is `None` rather than a silent fall back to
    /// `valid`: a mistyped filter that quietly answered "the valid ones" would
    /// look exactly like a bank with no corrections in it.
    pub fn parse(s: &str) -> Option<Self> {
        match s.trim() {
            "" | "valid" => Some(RecallState::Valid),
            "invalidated" => Some(RecallState::Invalidated),
            "all" => Some(RecallState::All),
            _ => None,
        }
    }

    /// Whether a record carrying `state` belongs in the answer. An unknown state
    /// on disk is admitted only by [`All`](RecallState::All): it is neither
    /// valid nor a correction, and a recall must never widen into "whatever the
    /// file said".
    pub fn admits(&self, state: &str) -> bool {
        match self {
            RecallState::Valid => state == STATE_VALID,
            RecallState::Invalidated => state == STATE_INVALIDATED,
            RecallState::All => true,
        }
    }
}

/// The most content one `retain` may store, in bytes. §5.1's payload is a
/// *constructed record, never a transcript*; this is the backstop that keeps an
/// agent from pasting one in anyway. Content past the cap is truncated, never
/// rejected — a retain is best-effort and never fatal (§5.1).
pub const MAX_RETAIN_CONTENT_BYTES: usize = 4000;

/// The most content ONE recalled fact contributes to a prompt, in bytes.
/// Applied at read time so a bank written before a cap change still renders
/// within bounds.
pub const MAX_FACT_CONTENT_BYTES: usize = 600;

/// The most a whole rendered memory section may contribute to the turn-1 prompt,
/// in bytes (§0.11.6's per-section cap). Enforced by the renderer that owns the
/// prompt, not by the bank; exported here so the bound lives beside the two it
/// composes with.
pub const MAX_SECTION_BYTES: usize = 4000;

/// The most record files one recall will read before giving up on the rest. A
/// bank is append-only and never compacted, so this is what stops a
/// years-old bank turning every dispatch into an unbounded directory walk.
/// Files are considered newest-first (record ids sort chronologically), so the
/// cap drops the OLDEST records, which are the ones least likely to score.
pub const MAX_BANK_SCAN: usize = 500;

/// The default `recall_top_k` used when `memory.recall_top_k` is absent or
/// non-positive. Restated from [`crate::teams`]'s schema default because the
/// point here is the FALLBACK: `recall_top_k: 0` must not silently mean "recall
/// nothing" in one place and "recall everything" in another.
pub const FALLBACK_TOP_K: usize = 8;

/// What the **host** stamps and the bank writes (§5.1). The agent supplies
/// [`content`](Record::content) and nothing else: `ticket`, `run_id` and
/// `identity` come from the run, `commit_sha` and `pr` from the workspace. "The
/// agent never has to remember to attach provenance, which is the part of a
/// discipline that erodes first" — and, equally, cannot forge it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Record {
    /// The teammate this record belongs to. Selects the bank directory.
    pub identity: String,
    /// §5.1's `document_id: run-<run_id>`.
    pub document_id: String,
    /// The ticket identifier the run was working (`STUDIO-645`). Re-grounded at
    /// recall (§5.2).
    pub ticket: String,
    /// The workspace HEAD at retain time. Empty when the daemon could not
    /// resolve it; never agent-supplied.
    pub commit_sha: String,
    /// The pull request, when known. Empty when unknown.
    pub pr: String,
    /// The run this retain came from.
    pub run_id: String,
    /// When the host stamped it. The bank has no clock of its own — passing the
    /// time in is what makes a stored record reproducible in a test.
    pub at: DateTime<Utc>,
    /// The agent's prose: observations and outcomes only (§5.1).
    pub content: String,
}

/// One record read back out of a bank.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct Fact {
    /// The record's stable id — its filename stem, and the `fact_id` an
    /// invalidate names.
    pub id: String,
    pub identity: String,
    pub document_id: String,
    pub ticket: String,
    pub commit_sha: String,
    pub pr: String,
    pub run_id: String,
    pub at: String,
    /// [`STATE_VALID`] or [`STATE_INVALIDATED`].
    pub state: String,
    /// Why it was invalidated (§5.3 stores the reason); empty while valid.
    pub reason: String,
    /// The record body, truncated to [`MAX_FACT_CONTENT_BYTES`].
    pub content: String,
}

/// What a recall matches records against (§5.2). Plain data — a recall never
/// sees an `Issue`, a tracker or a clock.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Query {
    /// The ticket identifier being dispatched, when there is one.
    pub ticket: String,
    /// The ticket's labels (or, for the `teams_recall` tool, nothing).
    pub labels: Vec<String>,
    /// The ticket title, or the tool's free-text query.
    pub title: String,
    /// `memory.recall_top_k`. Zero or negative ⇒ [`FALLBACK_TOP_K`].
    pub top_k: usize,
    /// **Browse:** every valid record matches, still ordered and still bounded
    /// by `top_k` (STUDIO-652).
    ///
    /// It exists because "show me what this teammate remembers" is not a search
    /// and has no query to score against: with all three match fields empty
    /// every record scores 0, and a zero score is a drop — so the obvious
    /// spelling of that question returns an empty bank that looks broken. This
    /// flag makes the answer "everything, bounded" instead of "nothing", which
    /// is what §5.2.3 needs before a wrong fact can be *noticed* and
    /// invalidated.
    ///
    /// **The dispatch path never sets it** — `teamscompose::recall_query`
    /// passes `browse: false` explicitly — so turn-1 recall keeps scoring
    /// exactly as before and no prompt gains a byte. Only the off-loop
    /// `teams_recall` browse surface turns it on, and only when the free-text
    /// query is empty.
    pub browse: bool,
    /// Which record states may come back (STUDIO-689). Defaults to
    /// [`RecallState::Valid`], which is what every pre-existing caller — the
    /// dispatch path included — keeps getting.
    pub state: RecallState,
}

impl Query {
    /// The effective cap, with the non-positive fallback applied.
    fn effective_top_k(&self) -> usize {
        if self.top_k == 0 {
            FALLBACK_TOP_K
        } else {
            self.top_k
        }
    }
}

/// What a recall produced: the facts, plus the record files that could not be
/// parsed.
///
/// The skipped list exists because "a corrupt record file is skipped **loudly**,
/// never fatal" is an acceptance criterion and `rhapsody-config` deliberately
/// does no logging of its own (the same convention [`crate::teams`] and
/// [`crate::profiles`] follow). The reason travels to the caller that owns the
/// log rather than being swallowed here.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Recalled {
    pub facts: Vec<Fact>,
    /// `(file name, why)` for each record that could not be read or parsed.
    pub skipped: Vec<(String, String)>,
}

/// Why a memory operation failed. Sentinel prefixes follow
/// [`TeamsError`](crate::teams::TeamsError)'s convention, so a caller can log
/// the reason verbatim.
#[derive(thiserror::Error, Debug)]
pub enum MemoryError {
    #[error("memory_io_error: {0}")]
    Io(String),
    #[error("memory_invalid: {0}")]
    Invalid(String),
    #[error("memory_not_found: {0}")]
    NotFound(String),
}

/// The pluggable memory backend (§5) — **the off-loop surface**.
///
/// `async` because T8's `hindsight` implementation does network I/O. See the
/// module docs: that is exactly why no caller on the dispatch path may hold one.
#[async_trait]
pub trait MemoryBackend: Send + Sync {
    /// Store a host-stamped record. Best-effort at every call site: a failed
    /// retain logs and the run completes (§5.1). Returns the new record's id.
    async fn retain(&self, rec: &Record) -> Result<String, MemoryError>;

    /// The identity's records matching `q` in the states [`Query::state`]
    /// admits, bounded by [`Query::top_k`]. Never creates anything.
    async fn recall(&self, identity: &str, q: &Query) -> Result<Recalled, MemoryError>;

    /// Mark one record non-valid, storing `reason` (§5.3). Reversible: nothing
    /// is deleted. `Ok(false)` ⇒ the record was already invalidated.
    async fn invalidate(
        &self,
        identity: &str,
        fact_id: &str,
        reason: &str,
    ) -> Result<bool, MemoryError>;

    /// Put one invalidated record back into recall (STUDIO-689). `Ok(false)` ⇒
    /// it was already valid, so nothing changed.
    ///
    /// The reversal §5.3 requires, on the trait rather than only on the two
    /// concrete banks: an invalidation that cannot be undone through the same
    /// surface that made it is reversible only in principle. It carries no
    /// `reason` — the stored one is dropped with the correction it explained,
    /// which is what makes the record identical to one that was never
    /// invalidated.
    async fn revalidate(&self, identity: &str, fact_id: &str) -> Result<bool, MemoryError>;
}

/// `memory.backend: none` — routing and profiles with no memory at all (§5.4).
/// Every method is a no-op that creates nothing and can fail in no way.
#[derive(Debug, Clone, Copy, Default)]
pub struct NoneBackend;

#[async_trait]
impl MemoryBackend for NoneBackend {
    async fn retain(&self, _rec: &Record) -> Result<String, MemoryError> {
        Ok(String::new())
    }

    async fn recall(&self, _identity: &str, _q: &Query) -> Result<Recalled, MemoryError> {
        Ok(Recalled::default())
    }

    async fn invalidate(
        &self,
        _identity: &str,
        _fact_id: &str,
        _reason: &str,
    ) -> Result<bool, MemoryError> {
        Ok(false)
    }

    async fn revalidate(&self, _identity: &str, _fact_id: &str) -> Result<bool, MemoryError> {
        Ok(false)
    }
}

/// The front matter of one stored record — the typed mirror of what a human
/// reads at the top of the file.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct FrontMatter {
    #[serde(default)]
    identity: String,
    #[serde(default)]
    document_id: String,
    #[serde(default)]
    ticket: String,
    #[serde(default)]
    commit_sha: String,
    #[serde(default)]
    pr: String,
    #[serde(default)]
    run_id: String,
    #[serde(default)]
    at: String,
    #[serde(default)]
    state: String,
    #[serde(default)]
    reason: String,
}

/// `memory.backend: local` — append-only markdown records, one file per record,
/// under `<root>/<bank_prefix><identity>/` (§5.4).
///
/// **Every method here is a plain `fn` over local files**, which is what lets
/// the dispatch path hold a `LocalBank` concretely and stay clearable by
/// signature (module docs). The [`MemoryBackend`] impl below simply delegates,
/// so the off-loop endpoints and the dispatch path read exactly the same bytes
/// through exactly the same code.
#[derive(Debug, Clone)]
pub struct LocalBank {
    root: PathBuf,
    bank_prefix: String,
    /// Per-identity bank-id overrides, from the roster's `bank:` field. Empty
    /// for the common case, where every identity derives its bank from
    /// `bank_prefix`.
    banks: std::collections::HashMap<String, String>,
}

impl LocalBank {
    /// Names a bank root. **Creates nothing** — not the root, not a bank
    /// directory. The first [`retain`](LocalBank::retain) does that.
    pub fn new(root: impl Into<PathBuf>, bank_prefix: impl Into<String>) -> Self {
        Self {
            root: root.into(),
            bank_prefix: bank_prefix.into(),
            banks: std::collections::HashMap::new(),
        }
    }

    /// Honours the roster's per-identity `bank:` overrides (§2.2: "Memory bank
    /// id; empty ⇒ `<memory.bank_prefix><name>`").
    ///
    /// Without this the override would be a field the roster VIEW reports and
    /// the store ignores — `teams_roster` would name `custom` while records
    /// landed in `agent-alice`. An override that is not label-safe is dropped
    /// rather than joined: it becomes a directory name, so it can no more carry
    /// a separator than an identity can.
    pub fn with_bank_overrides<I, K, V>(mut self, overrides: I) -> Self
    where
        I: IntoIterator<Item = (K, V)>,
        K: Into<String>,
        V: Into<String>,
    {
        for (identity, bank) in overrides {
            let (identity, bank) = (identity.into(), bank.into());
            if !bank.is_empty() && crate::teams::is_label_safe(&bank) {
                self.banks.insert(identity, bank);
            }
        }
        self
    }

    /// The bank root every identity's directory sits under.
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// The directory holding `identity`'s records: `<root>/<bank_prefix><name>`.
    /// Naming it does not create it.
    ///
    /// `identity` is validated as label-safe by
    /// [`Teams::validate`](crate::teams::Teams) before it can reach a roster, but
    /// this is also reached from an MCP tool argument, so the charset is checked
    /// again here rather than trusted — a `..` would otherwise escape the root.
    pub fn bank_dir(&self, identity: &str) -> Result<PathBuf, MemoryError> {
        if !crate::teams::is_label_safe(identity) {
            return Err(MemoryError::Invalid(format!(
                "identity {identity:?} is not label-safe (must match ^[a-z][a-z0-9-]*$)"
            )));
        }
        // The roster's `bank:` override wins; every entry in the map was already
        // charset-checked by `with_bank_overrides`.
        Ok(match self.banks.get(identity) {
            Some(bank) => self.root.join(bank),
            None => self.root.join(format!("{}{identity}", self.bank_prefix)),
        })
    }

    /// The bank id `identity`'s records live under — the override when the
    /// roster set one, else `<bank_prefix><name>`. What `teams_roster` reports,
    /// so the view and the store cannot disagree.
    pub fn bank_id(&self, identity: &str) -> String {
        bank_id_for(&self.bank_prefix, &self.banks, identity)
    }

    /// Appends one host-stamped record and returns its id.
    ///
    /// **This is the only method in the module that creates anything**, and it
    /// creates the bank directory (and the root above it) on the way. Record ids
    /// are `<compact-utc>-<document_id>`, which sorts chronologically and names
    /// the run in the filename a human greps for; a collision within the same
    /// second gets a `-2`, `-3`, … suffix rather than overwriting a record,
    /// because the store is append-only.
    pub fn retain(&self, rec: &Record) -> Result<String, MemoryError> {
        let dir = self.bank_dir(&rec.identity)?;
        std::fs::create_dir_all(&dir)
            .map_err(|e| MemoryError::Io(format!("create bank {}: {e}", dir.display())))?;
        let stem = record_stem(rec);
        let (id, path) = self.unique_path(&dir, &stem)?;
        let front = FrontMatter {
            identity: rec.identity.clone(),
            document_id: rec.document_id.clone(),
            ticket: rec.ticket.clone(),
            commit_sha: rec.commit_sha.clone(),
            pr: rec.pr.clone(),
            run_id: rec.run_id.clone(),
            at: rec.at.to_rfc3339_opts(SecondsFormat::Secs, true),
            state: STATE_VALID.to_string(),
            reason: String::new(),
        };
        let text = render_record(
            &front,
            &truncate_bytes(&rec.content, MAX_RETAIN_CONTENT_BYTES),
        )?;
        std::fs::write(&path, text)
            .map_err(|e| MemoryError::Io(format!("write record {}: {e}", path.display())))?;
        Ok(id)
    }

    /// The identity's records matching `q` in the states [`Query::state`]
    /// admits — **valid only** unless the caller widened it — best-scoring
    /// first, truncated to [`Query::top_k`].
    ///
    /// Never creates anything: a bank directory that does not exist is an empty
    /// result, not an error and not a `mkdir`. A record file that cannot be read
    /// or parsed is skipped and reported in [`Recalled::skipped`] — one bad file
    /// never costs the caller the rest of the bank.
    pub fn recall(&self, identity: &str, q: &Query) -> Result<Recalled, MemoryError> {
        let dir = self.bank_dir(identity)?;
        let mut out = Recalled::default();
        let entries = match std::fs::read_dir(&dir) {
            Ok(e) => e,
            // The bank has never been written. Not an error, and emphatically
            // not a reason to create it.
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(out),
            Err(e) => {
                return Err(MemoryError::Io(format!("read bank {}: {e}", dir.display())));
            }
        };
        let suffix = format!(".{RECORD_EXT}");
        let mut names: Vec<String> = Vec::new();
        for entry in entries {
            let entry = match entry {
                Ok(e) => e,
                Err(e) => {
                    out.skipped.push((String::new(), e.to_string()));
                    continue;
                }
            };
            let name = entry.file_name().to_string_lossy().into_owned();
            if !name.ends_with(&suffix) {
                continue;
            }
            names.push(name);
        }
        // Newest first, so `MAX_BANK_SCAN` drops the oldest records rather than
        // whichever ones the filesystem happened to hand back last.
        names.sort_unstable_by(|a, b| b.cmp(a));
        names.truncate(MAX_BANK_SCAN);

        let mut scored: Vec<(i64, Fact)> = Vec::new();
        for name in names {
            let path = dir.join(&name);
            let text = match std::fs::read_to_string(&path) {
                Ok(t) => t,
                Err(e) => {
                    out.skipped.push((name, e.to_string()));
                    continue;
                }
            };
            // `strip_suffix`, NOT `trim_end_matches`: the latter strips EVERY
            // trailing repetition, so a hand-dropped `notes.md.md` would report
            // the id `notes` — and an invalidate naming that id would then miss
            // the file it came from, or hit a different one.
            let id = name.strip_suffix(&suffix).unwrap_or(&name).to_string();
            let fact = match parse_record(&id, &text) {
                Ok(f) => f,
                Err(e) => {
                    out.skipped.push((name, e.to_string()));
                    continue;
                }
            };
            if !q.state.admits(&fact.state) {
                continue;
            }
            let score = score_fact(&fact, q);
            if score > 0 {
                scored.push((score, fact));
            }
        }
        // Highest score first, then newest, then by id — a total order, so two
        // dispatches of the same ticket render the same section.
        scored.sort_by(|a, b| {
            b.0.cmp(&a.0)
                .then_with(|| b.1.at.cmp(&a.1.at))
                .then_with(|| a.1.id.cmp(&b.1.id))
        });
        out.facts = scored
            .into_iter()
            .take(q.effective_top_k())
            .map(|(_, f)| f)
            .collect();
        Ok(out)
    }

    /// Marks one record [`STATE_INVALIDATED`] with `reason` (§5.3). Returns
    /// `false` when it was already invalidated.
    pub fn invalidate(
        &self,
        identity: &str,
        fact_id: &str,
        reason: &str,
    ) -> Result<bool, MemoryError> {
        self.set_state(identity, fact_id, STATE_INVALIDATED, reason)
    }

    /// The reversal §5.3 requires: nothing was deleted, so flipping the flag
    /// back to [`STATE_VALID`] restores the record to recall. Exposed as its own
    /// method so "reversible" is a property the code has, not a property the
    /// file format merely permits.
    pub fn revalidate(&self, identity: &str, fact_id: &str) -> Result<bool, MemoryError> {
        self.set_state(identity, fact_id, STATE_VALID, "")
    }

    /// Rewrites one record's `state` + `reason` front matter, preserving its
    /// body verbatim.
    pub fn set_state(
        &self,
        identity: &str,
        fact_id: &str,
        state: &str,
        reason: &str,
    ) -> Result<bool, MemoryError> {
        let dir = self.bank_dir(identity)?;
        let path = record_path(&dir, fact_id)?;
        let text = std::fs::read_to_string(&path).map_err(|e| {
            if e.kind() == std::io::ErrorKind::NotFound {
                MemoryError::NotFound(format!("no record {fact_id:?} for identity {identity:?}"))
            } else {
                MemoryError::Io(format!("read record {}: {e}", path.display()))
            }
        })?;
        let (mut front, body) = split_record(&text)?;
        if front.state == state {
            return Ok(false);
        }
        front.state = state.to_string();
        front.reason = reason.to_string();
        let rendered = render_record(&front, &body)?;
        std::fs::write(&path, rendered)
            .map_err(|e| MemoryError::Io(format!("write record {}: {e}", path.display())))?;
        Ok(true)
    }

    /// The first free `<stem>.md` / `<stem>-2.md` / … in `dir`, as `(id, path)`.
    fn unique_path(&self, dir: &Path, stem: &str) -> Result<(String, PathBuf), MemoryError> {
        for n in 1..=1000u32 {
            let id = if n == 1 {
                stem.to_string()
            } else {
                format!("{stem}-{n}")
            };
            let path = dir.join(format!("{id}.{RECORD_EXT}"));
            if !path.exists() {
                return Ok((id, path));
            }
        }
        Err(MemoryError::Io(format!(
            "bank {} already holds 1000 records for {stem}",
            dir.display()
        )))
    }
}

#[async_trait]
impl MemoryBackend for LocalBank {
    async fn retain(&self, rec: &Record) -> Result<String, MemoryError> {
        LocalBank::retain(self, rec)
    }

    async fn recall(&self, identity: &str, q: &Query) -> Result<Recalled, MemoryError> {
        LocalBank::recall(self, identity, q)
    }

    async fn invalidate(
        &self,
        identity: &str,
        fact_id: &str,
        reason: &str,
    ) -> Result<bool, MemoryError> {
        LocalBank::invalidate(self, identity, fact_id, reason)
    }

    async fn revalidate(&self, identity: &str, fact_id: &str) -> Result<bool, MemoryError> {
        LocalBank::revalidate(self, identity, fact_id)
    }
}

/// The bank id `identity`'s records live under: the roster's `bank:` override
/// when there is one, else `<bank_prefix><name>` (§2.2).
///
/// A free function rather than a method because **every backend must resolve a
/// bank id the same way** — [`LocalBank::bank_id`] names a directory,
/// [`HindsightBackend`](crate::hindsight::HindsightBackend) names a path segment
/// in a URL, and [`Cursors`](crate::room::Cursors) names a sibling of the bank —
/// and three copies of a two-line `match` is exactly how a roster override ends
/// up honoured by the view and ignored by the store.
///
/// It does **not** validate `identity`: the callers do, because what an invalid
/// identity would escape into differs (a directory, a URL path), and each of
/// them refuses it before ever asking for a bank id.
pub fn bank_id_for(
    bank_prefix: &str,
    overrides: &std::collections::HashMap<String, String>,
    identity: &str,
) -> String {
    match overrides.get(identity) {
        Some(bank) => bank.clone(),
        None => format!("{bank_prefix}{identity}"),
    }
}

/// The record id stem: `<compact-utc>-<document_id-or-run>`, e.g.
/// `20260829T174500Z-run-412`. Chronologically sortable and greppable by run.
fn record_stem(rec: &Record) -> String {
    let ts = rec.at.format("%Y%m%dT%H%M%SZ").to_string();
    let doc = sanitize_id(&rec.document_id);
    if doc.is_empty() {
        ts
    } else {
        format!("{ts}-{doc}")
    }
}

/// Reduces a host-supplied id fragment to `[a-z0-9-]`, so a record filename can
/// never carry a separator or a traversal even if a future caller stamps
/// something unexpected into `document_id`.
fn sanitize_id(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        if c.is_ascii_alphanumeric() {
            out.push(c.to_ascii_lowercase());
        } else if !out.ends_with('-') {
            out.push('-');
        }
    }
    out.trim_matches('-').to_string()
}

/// Resolves `<dir>/<fact_id>.md`, rejecting any id that is not a plain record
/// stem. `fact_id` arrives from an MCP tool argument, so a separator or a `..`
/// must be refused rather than joined.
fn record_path(dir: &Path, fact_id: &str) -> Result<PathBuf, MemoryError> {
    // `.` is allowed because it is REACHABLE: a bank is a directory of files a
    // human may add to, and `recall` reports the filename-minus-one-extension as
    // the id — so a hand-written `notes.md.md` is reported as `notes.md`, and an
    // id this function refused would be one no caller could ever act on. What
    // must not get through is anything that could leave the bank directory.
    let ok = !fact_id.is_empty()
        && !fact_id.contains("..")
        && fact_id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'));
    if !ok {
        return Err(MemoryError::Invalid(format!(
            "fact_id {fact_id:?} is not a record id (expected [A-Za-z0-9_-.]+ with no \"..\")"
        )));
    }
    Ok(dir.join(format!("{fact_id}.{RECORD_EXT}")))
}

/// Serializes one record file: `---` front matter `---` then the body.
fn render_record(front: &FrontMatter, body: &str) -> Result<String, MemoryError> {
    let yaml = serde_yaml_ng::to_string(front)
        .map_err(|e| MemoryError::Invalid(format!("encode front matter: {e}")))?;
    Ok(format!("---\n{yaml}---\n\n{}\n", body.trim_end()))
}

/// Splits a record file into its typed front matter and its body.
fn split_record(text: &str) -> Result<(FrontMatter, String), MemoryError> {
    let rest = text
        .strip_prefix("---\n")
        .ok_or_else(|| MemoryError::Invalid("record has no front matter".to_string()))?;
    let end = rest
        .find("\n---\n")
        .ok_or_else(|| MemoryError::Invalid("record front matter is not terminated".to_string()))?;
    let front: FrontMatter = serde_yaml_ng::from_str(&rest[..end + 1])
        .map_err(|e| MemoryError::Invalid(format!("parse front matter: {e}")))?;
    let body = rest[end + "\n---\n".len()..].trim().to_string();
    Ok((front, body))
}

/// Parses one stored record into a [`Fact`], applying the read-time content cap.
fn parse_record(id: &str, text: &str) -> Result<Fact, MemoryError> {
    let (front, body) = split_record(text)?;
    if front.identity.is_empty() {
        return Err(MemoryError::Invalid("record names no identity".to_string()));
    }
    Ok(Fact {
        id: id.to_string(),
        identity: front.identity,
        document_id: front.document_id,
        ticket: front.ticket,
        commit_sha: front.commit_sha,
        pr: front.pr,
        run_id: front.run_id,
        at: front.at,
        // An older record written before the flag existed reads as valid: the
        // absence of an invalidation is not an invalidation.
        state: if front.state.is_empty() {
            STATE_VALID.to_string()
        } else {
            front.state
        },
        reason: front.reason,
        content: truncate_bytes(&body, MAX_FACT_CONTENT_BYTES),
    })
}

/// How well one fact answers `q` (§5.2's "tag/keyword" lookup). Zero ⇒ the fact
/// is unrelated and is not recalled at all — an unrelated fact is pure turn-1
/// cost.
fn score_fact(f: &Fact, q: &Query) -> i64 {
    let hay = format!("{} {}", f.ticket, f.content).to_ascii_lowercase();
    // A browse floors every valid record at 1 so nothing is dropped for scoring zero; the
    // match terms below still rank, so a browse WITH terms reads as "everything, best first".
    let mut score = i64::from(q.browse);
    if !q.ticket.is_empty() {
        let ticket = q.ticket.to_ascii_lowercase();
        if f.ticket.to_ascii_lowercase() == ticket {
            score += 5;
        } else if hay.contains(&ticket) {
            score += 3;
        }
    }
    for label in &q.labels {
        let l = label.trim().to_ascii_lowercase();
        if l.len() >= 2 && hay.contains(&l) {
            score += 2;
        }
    }
    for token in q
        .title
        .to_ascii_lowercase()
        .split(|c: char| !c.is_alphanumeric())
    {
        if token.len() >= 4 && hay.contains(token) {
            score += 1;
        }
    }
    score
}

/// Truncates `s` to at most `max` bytes without splitting a UTF-8 character,
/// appending an ellipsis marker when anything was dropped so a reader can tell.
pub fn truncate_bytes(s: &str, max: usize) -> String {
    if s.len() <= max {
        return s.to_string();
    }
    const MARK: &str = "…";
    let budget = max.saturating_sub(MARK.len());
    let mut end = budget.min(s.len());
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}{MARK}", &s[..end])
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bank(dir: &Path) -> LocalBank {
        LocalBank::new(dir.join(DEFAULT_BANKS_SUBDIR), "agent-")
    }

    fn at(secs: i64) -> DateTime<Utc> {
        DateTime::from_timestamp(1_756_000_000 + secs, 0).expect("timestamp")
    }

    fn record(identity: &str, ticket: &str, run: &str, content: &str) -> Record {
        Record {
            identity: identity.to_string(),
            document_id: format!("run-{run}"),
            ticket: ticket.to_string(),
            commit_sha: "abc1234".to_string(),
            pr: "42".to_string(),
            run_id: run.to_string(),
            at: at(0),
            content: content.to_string(),
        }
    }

    fn ticket_query(ticket: &str) -> Query {
        Query {
            ticket: ticket.to_string(),
            top_k: 8,
            ..Query::default()
        }
    }

    /// The T1/T2 rule, carried into memory: **loading and recalling create
    /// nothing.** A bank dir appears on the first retain and at no other time,
    /// which is what makes "Teams off costs nothing" true of the filesystem and
    /// not merely of the code path (§2.4 row 8).
    #[test]
    fn recall_on_a_missing_bank_creates_nothing() {
        let dir = tempfile::tempdir().expect("tempdir");
        let b = bank(dir.path());
        assert!(!b.root().exists());

        let got = b
            .recall("alice", &ticket_query("STUDIO-1"))
            .expect("recall");
        assert_eq!(got, Recalled::default());
        assert!(!b.root().exists(), "recall created {}", b.root().display());
        assert!(
            !b.bank_dir("alice").expect("bank dir").exists(),
            "recall created alice's bank"
        );
    }

    /// A retain lands as a front-matter record a human can read, carrying the
    /// provenance the HOST stamped (§5.1) — and it is the first thing that
    /// creates the bank.
    #[test]
    fn retain_writes_a_front_matter_record_and_creates_the_bank() {
        let dir = tempfile::tempdir().expect("tempdir");
        let b = bank(dir.path());
        let id = b
            .retain(&record(
                "alice",
                "STUDIO-645",
                "412",
                "The poller skips null attachments.",
            ))
            .expect("retain");

        let path = b
            .bank_dir("alice")
            .expect("bank dir")
            .join(format!("{id}.{RECORD_EXT}"));
        let text = std::fs::read_to_string(&path).expect("record readable");
        assert!(
            text.starts_with("---\n"),
            "record must open front matter: {text:?}"
        );
        for expected in [
            "identity: alice",
            "document_id: run-412",
            "ticket: STUDIO-645",
            "commit_sha: abc1234",
            "run_id: '412'",
            "state: valid",
        ] {
            assert!(
                text.contains(expected),
                "record must carry {expected:?}: {text:?}"
            );
        }
        assert!(
            text.contains("The poller skips null attachments."),
            "record must carry the agent's prose: {text:?}"
        );
        assert!(id.contains("run-412"), "the id names the run: {id:?}");
    }

    /// Recall returns what retain stored, bounded by `top_k` and ranked
    /// best-first. The store is append-only, so two retains from one run are two
    /// records, never an overwrite.
    #[test]
    fn recall_returns_retained_facts_bounded_by_top_k() {
        let dir = tempfile::tempdir().expect("tempdir");
        let b = bank(dir.path());
        for n in 0..5 {
            let mut r = record("alice", "STUDIO-645", "412", &format!("observation {n}"));
            r.at = at(n);
            b.retain(&r).expect("retain");
        }
        let all = b
            .recall("alice", &ticket_query("STUDIO-645"))
            .expect("recall");
        assert_eq!(all.facts.len(), 5, "every matching record is recallable");
        assert!(all.skipped.is_empty());

        let bounded = b
            .recall(
                "alice",
                &Query {
                    ticket: "STUDIO-645".to_string(),
                    top_k: 2,
                    ..Query::default()
                },
            )
            .expect("recall");
        assert_eq!(bounded.facts.len(), 2, "top_k caps the result");
        assert_eq!(
            bounded.facts[0].content, "observation 4",
            "newest first among equal scores"
        );
    }

    /// **A browse is "everything, bounded" — not "nothing"** (STUDIO-652).
    ///
    /// With no ticket, no labels and no title there is nothing to score against, so every
    /// record scores 0 and the zero-score drop empties the result. That is the correct answer
    /// to a *search* and the wrong answer to "what does this teammate remember", which is the
    /// question the dashboard's memory listing asks. `browse` floors the score at 1, and the
    /// existing `top_k` + newest-first ordering still bound what comes back.
    #[test]
    fn a_browse_recalls_the_whole_bank_bounded_and_newest_first() {
        let dir = tempfile::tempdir().expect("tempdir");
        let b = bank(dir.path());
        for n in 0..5 {
            let mut r = record(
                "alice",
                &format!("OTHER-{n}"),
                &n.to_string(),
                &format!("observation {n}"),
            );
            r.at = at(n);
            b.retain(&r).expect("retain");
        }

        // The same empty query WITHOUT browse is still empty — the dispatch path's behaviour
        // is untouched, which is the whole reason this is a flag and not a rule.
        let searched = b
            .recall(
                "alice",
                &Query {
                    top_k: 8,
                    ..Query::default()
                },
            )
            .expect("recall");
        assert!(
            searched.facts.is_empty(),
            "an empty SEARCH still matches nothing: {searched:?}"
        );

        let browsed = b
            .recall(
                "alice",
                &Query {
                    top_k: 8,
                    browse: true,
                    ..Query::default()
                },
            )
            .expect("recall");
        assert_eq!(browsed.facts.len(), 5, "a browse returns the whole bank");

        let bounded = b
            .recall(
                "alice",
                &Query {
                    top_k: 2,
                    browse: true,
                    ..Query::default()
                },
            )
            .expect("recall");
        assert_eq!(bounded.facts.len(), 2, "top_k still bounds a browse");
        assert_eq!(
            bounded.facts[0].content, "observation 4",
            "newest first among equal scores"
        );
    }

    /// A browse shows only VALID records, so invalidating a fact from the dashboard makes it
    /// leave the listing — the round-trip §5.2.3 asks the button to close.
    #[test]
    fn a_browse_hides_an_invalidated_record() {
        let dir = tempfile::tempdir().expect("tempdir");
        let b = bank(dir.path());
        let id = b
            .retain(&record("alice", "OTHER-1", "1", "never was true"))
            .expect("retain");
        let browse = Query {
            top_k: 8,
            browse: true,
            ..Query::default()
        };
        assert_eq!(b.recall("alice", &browse).expect("recall").facts.len(), 1);

        b.invalidate("alice", &id, "measured otherwise")
            .expect("invalidate");
        assert!(
            b.recall("alice", &browse).expect("recall").facts.is_empty(),
            "an invalidated record leaves the browse listing"
        );
    }

    /// An unrelated fact is not recalled at all: every recalled byte is turn-1
    /// prompt cost on every future run (§0.5), so a zero score is a drop.
    #[test]
    fn an_unrelated_fact_is_not_recalled() {
        let dir = tempfile::tempdir().expect("tempdir");
        let b = bank(dir.path());
        b.retain(&record("alice", "OTHER-1", "9", "nothing to do with it"))
            .expect("retain");
        let got = b
            .recall("alice", &ticket_query("STUDIO-645"))
            .expect("recall");
        assert!(
            got.facts.is_empty(),
            "unrelated facts must not be recalled: {got:?}"
        );
    }

    /// Labels and title words find a record that does not name the ticket —
    /// §5.2's tag/keyword lookup.
    #[test]
    fn labels_and_title_words_match_records() {
        let dir = tempfile::tempdir().expect("tempdir");
        let b = bank(dir.path());
        b.retain(&record(
            "alice",
            "STUDIO-1",
            "1",
            "The dispatch path must never await.",
        ))
        .expect("retain");

        let by_label = b
            .recall(
                "alice",
                &Query {
                    labels: vec!["dispatch".to_string()],
                    top_k: 8,
                    ..Query::default()
                },
            )
            .expect("recall");
        assert_eq!(by_label.facts.len(), 1, "a label match recalls the record");

        let by_title = b
            .recall(
                "alice",
                &Query {
                    title: "keep the dispatch path sync".to_string(),
                    top_k: 8,
                    ..Query::default()
                },
            )
            .expect("recall");
        assert_eq!(by_title.facts.len(), 1, "a title word recalls the record");
    }

    /// §5.3: invalidation removes a fact from recall, stores the reason, deletes
    /// nothing, and is reversible.
    #[test]
    fn invalidate_removes_from_recall_reversibly_with_the_reason_kept() {
        let dir = tempfile::tempdir().expect("tempdir");
        let b = bank(dir.path());
        let id = b
            .retain(&record(
                "alice",
                "STUDIO-645",
                "412",
                "STUDIO-408 is still open.",
            ))
            .expect("retain");

        assert!(
            b.invalidate("alice", &id, "STUDIO-408 was Done on 2026-08-19")
                .expect("invalidate"),
            "the first invalidate changes state"
        );
        assert!(
            !b.invalidate("alice", &id, "again").expect("invalidate"),
            "invalidating twice is a no-op, not an error"
        );

        let after = b
            .recall("alice", &ticket_query("STUDIO-645"))
            .expect("recall");
        assert!(
            after.facts.is_empty(),
            "an invalidated fact is invisible to recall"
        );

        let path = b
            .bank_dir("alice")
            .expect("bank dir")
            .join(format!("{id}.{RECORD_EXT}"));
        let text = std::fs::read_to_string(&path).expect("the record still exists");
        assert!(text.contains("state: invalidated"), "{text:?}");
        assert!(
            text.contains("STUDIO-408 was Done on 2026-08-19"),
            "the reason is kept: {text:?}"
        );
        assert!(
            text.contains("STUDIO-408 is still open."),
            "the body is kept verbatim: {text:?}"
        );

        assert!(b.revalidate("alice", &id).expect("revalidate"));
        let back = b
            .recall("alice", &ticket_query("STUDIO-645"))
            .expect("recall");
        assert_eq!(back.facts.len(), 1, "invalidation is reversible");
    }

    /// A corrupt record file is skipped LOUDLY — reported to the caller that
    /// owns the log — and never costs the rest of the bank.
    #[test]
    fn a_corrupt_record_is_skipped_loudly_and_never_fatal() {
        let dir = tempfile::tempdir().expect("tempdir");
        let b = bank(dir.path());
        b.retain(&record("alice", "STUDIO-645", "412", "a good record"))
            .expect("retain");
        let bank_dir = b.bank_dir("alice").expect("bank dir");
        std::fs::write(
            bank_dir.join("00000000T000000Z-broken.md"),
            "not a record at all",
        )
        .expect("write corrupt");

        let got = b
            .recall("alice", &ticket_query("STUDIO-645"))
            .expect("recall");
        assert_eq!(got.facts.len(), 1, "the good record still comes back");
        assert_eq!(got.skipped.len(), 1, "the corrupt file is reported");
        assert_eq!(got.skipped[0].0, "00000000T000000Z-broken.md");
        assert!(
            got.skipped[0].1.contains("front matter"),
            "the reason must say why: {:?}",
            got.skipped[0].1
        );
    }

    /// A bank survives a restart because it is files: a second `LocalBank` over
    /// the same root reads what the first one wrote.
    #[test]
    fn a_bank_survives_a_new_backend_over_the_same_root() {
        let dir = tempfile::tempdir().expect("tempdir");
        bank(dir.path())
            .retain(&record("alice", "STUDIO-645", "412", "survives a restart"))
            .expect("retain");
        let reopened = bank(dir.path());
        let got = reopened
            .recall("alice", &ticket_query("STUDIO-645"))
            .expect("recall");
        assert_eq!(got.facts.len(), 1);
        assert_eq!(got.facts[0].content, "survives a restart");
    }

    /// Neither an identity nor a `fact_id` may escape the bank root. Both arrive
    /// from MCP tool arguments, so both are checked rather than trusted.
    #[test]
    fn traversal_in_identity_or_fact_id_is_refused() {
        let dir = tempfile::tempdir().expect("tempdir");
        let b = bank(dir.path());
        for bad in ["../etc", "Alice", "", "a/b"] {
            assert!(b.bank_dir(bad).is_err(), "identity {bad:?} must be refused");
        }
        b.retain(&record("alice", "STUDIO-1", "1", "x"))
            .expect("retain");
        for bad in [
            "../../etc/passwd",
            "a/b",
            "",
            "..",
            "a..b",
            "x/../y",
            "a\\b",
        ] {
            match b.invalidate("alice", bad, "why") {
                Err(MemoryError::Invalid(_)) => {}
                other => panic!("fact_id {bad:?} must be refused, got {other:?}"),
            }
        }
    }

    /// Content is capped on the way in and again on the way out, so neither a
    /// huge retain nor a bank written under a laxer cap can blow the turn-1
    /// prompt budget (§0.5).
    #[test]
    fn content_is_capped_when_stored_and_when_recalled() {
        let dir = tempfile::tempdir().expect("tempdir");
        let b = bank(dir.path());
        let huge = "STUDIO-645 ".repeat(2000);
        b.retain(&record("alice", "STUDIO-645", "412", &huge))
            .expect("retain");
        let got = b
            .recall("alice", &ticket_query("STUDIO-645"))
            .expect("recall");
        assert_eq!(got.facts.len(), 1);
        assert!(
            got.facts[0].content.len() <= MAX_FACT_CONTENT_BYTES,
            "a recalled fact is capped at {MAX_FACT_CONTENT_BYTES} bytes, got {}",
            got.facts[0].content.len()
        );
    }

    /// Two retains in the same second are two records, not an overwrite: the
    /// store is append-only.
    #[test]
    fn two_retains_in_one_second_are_two_records() {
        let dir = tempfile::tempdir().expect("tempdir");
        let b = bank(dir.path());
        let first = b
            .retain(&record("alice", "STUDIO-1", "1", "one"))
            .expect("retain");
        let second = b
            .retain(&record("alice", "STUDIO-1", "1", "two"))
            .expect("retain");
        assert_ne!(first, second, "the second retain must not reuse the id");
        let got = b
            .recall("alice", &ticket_query("STUDIO-1"))
            .expect("recall");
        assert_eq!(got.facts.len(), 2);
    }

    /// `none` is a no-op that creates nothing and cannot fail — §5.4's "routing
    /// and profiles with no memory".
    #[tokio::test]
    async fn none_backend_stores_and_recalls_nothing() {
        let b = NoneBackend;
        assert_eq!(
            b.retain(&record("alice", "S-1", "1", "x"))
                .await
                .expect("retain"),
            ""
        );
        assert_eq!(
            b.recall("alice", &ticket_query("S-1"))
                .await
                .expect("recall"),
            Recalled::default()
        );
        assert!(
            !b.invalidate("alice", "id", "why")
                .await
                .expect("invalidate")
        );
    }

    /// The async trait and the sync methods are the same code over the same
    /// bytes — the delegation is what lets the dispatch path hold the concrete
    /// type without a second implementation to keep in step.
    #[tokio::test]
    async fn the_trait_delegates_to_the_sync_bank() {
        let dir = tempfile::tempdir().expect("tempdir");
        let b = bank(dir.path());
        let backend: &dyn MemoryBackend = &b;
        let id = backend
            .retain(&record("alice", "STUDIO-645", "412", "through the trait"))
            .await
            .expect("retain");
        let sync_read = b
            .recall("alice", &ticket_query("STUDIO-645"))
            .expect("recall");
        assert_eq!(sync_read.facts.len(), 1);
        assert_eq!(sync_read.facts[0].id, id);
        assert!(
            backend
                .invalidate("alice", &id, "no longer true")
                .await
                .expect("invalidate")
        );
        assert!(
            b.recall("alice", &ticket_query("STUDIO-645"))
                .expect("recall")
                .facts
                .is_empty()
        );
        // The reversal is on the TRAIT, not only on the concrete bank
        // (STUDIO-689): the operator UI holds a `dyn MemoryBackend` and cannot
        // reach `LocalBank::revalidate`.
        assert!(
            backend
                .revalidate("alice", &id)
                .await
                .expect("revalidate through the trait")
        );
        assert_eq!(
            b.recall("alice", &ticket_query("STUDIO-645"))
                .expect("recall")
                .facts
                .len(),
            1,
            "a reinstated record is recalled again"
        );
        assert!(
            !backend
                .revalidate("alice", &id)
                .await
                .expect("revalidate twice"),
            "reinstating a valid record is a no-op, not an error"
        );
    }

    /// STUDIO-689: an invalidated record is *listable* on request, so the
    /// operator surface can show a correction made in an earlier session — while
    /// the default stays valid-only, which is what turn-1 recall asks for.
    #[test]
    fn recall_lists_invalidated_records_only_when_asked() {
        let dir = tempfile::tempdir().expect("tempdir");
        let b = bank(dir.path());
        let kept = b
            .retain(&record("alice", "STUDIO-689", "1", "still true"))
            .expect("retain");
        let dropped = b
            .retain(&record("alice", "STUDIO-689", "2", "never was true"))
            .expect("retain");
        assert!(
            b.invalidate("alice", &dropped, "the run it named was rolled back")
                .expect("invalidate")
        );

        let query = |state| Query {
            ticket: "STUDIO-689".to_string(),
            top_k: 8,
            state,
            ..Query::default()
        };

        let valid = b
            .recall("alice", &query(RecallState::Valid))
            .expect("recall");
        assert_eq!(
            valid
                .facts
                .iter()
                .map(|f| f.id.as_str())
                .collect::<Vec<_>>(),
            vec![kept.as_str()],
            "the default is unchanged: valid records only"
        );

        let corrected = b
            .recall("alice", &query(RecallState::Invalidated))
            .expect("recall");
        assert_eq!(
            corrected
                .facts
                .iter()
                .map(|f| f.id.as_str())
                .collect::<Vec<_>>(),
            vec![dropped.as_str()],
            "the corrections, and nothing else"
        );
        assert_eq!(corrected.facts[0].state, STATE_INVALIDATED);
        assert_eq!(
            corrected.facts[0].reason, "the run it named was rolled back",
            "the reason travels with the listed record, or the correction is unreadable"
        );

        let all = b.recall("alice", &query(RecallState::All)).expect("recall");
        let mut ids: Vec<&str> = all.facts.iter().map(|f| f.id.as_str()).collect();
        ids.sort_unstable();
        let mut want = vec![kept.as_str(), dropped.as_str()];
        want.sort_unstable();
        assert_eq!(ids, want, "`all` is the bank as it is on disk");
    }

    /// The wire spellings, and the one that must NOT be lenient: a mistyped
    /// filter answered with the valid records would read as a bank nobody has
    /// ever corrected.
    #[test]
    fn recall_state_parses_its_wire_spellings_and_refuses_the_rest() {
        assert_eq!(RecallState::parse(""), Some(RecallState::Valid));
        assert_eq!(RecallState::parse("  "), Some(RecallState::Valid));
        assert_eq!(RecallState::parse("valid"), Some(RecallState::Valid));
        assert_eq!(
            RecallState::parse("invalidated"),
            Some(RecallState::Invalidated)
        );
        assert_eq!(RecallState::parse("all"), Some(RecallState::All));
        assert_eq!(RecallState::parse("Valid"), None);
        assert_eq!(RecallState::parse("invalid"), None);
        assert_eq!(RecallState::default(), RecallState::Valid);
        assert_eq!(RecallState::All.as_str(), "all");
        assert!(!RecallState::Valid.admits("unknown-state"));
        assert!(RecallState::All.admits("unknown-state"));
    }

    /// The roster's `bank:` override selects the directory records actually
    /// live in — otherwise it would be a field the roster VIEW reports and the
    /// store ignores, and `teams_roster` would name one bank while records
    /// landed in another.
    #[test]
    fn a_roster_bank_override_selects_the_directory() {
        let dir = tempfile::tempdir().expect("tempdir");
        let b = LocalBank::new(dir.path().join(DEFAULT_BANKS_SUBDIR), "agent-")
            .with_bank_overrides([("alice", "shared-rust"), ("bob", "")]);

        assert_eq!(b.bank_id("alice"), "shared-rust");
        assert_eq!(
            b.bank_id("bob"),
            "agent-bob",
            "an empty override is no override"
        );
        assert!(
            b.bank_dir("alice")
                .expect("bank dir")
                .ends_with("shared-rust"),
            "{:?}",
            b.bank_dir("alice")
        );

        b.retain(&record("alice", "STUDIO-1", "1", "into the shared bank"))
            .expect("retain");
        assert!(
            dir.path()
                .join(DEFAULT_BANKS_SUBDIR)
                .join("shared-rust")
                .exists(),
            "the override directory is what retain creates"
        );
        assert_eq!(
            b.recall("alice", &ticket_query("STUDIO-1"))
                .expect("recall")
                .facts
                .len(),
            1,
            "and what recall reads back"
        );
    }

    /// An override that is not label-safe is DROPPED, not joined: it becomes a
    /// directory name under the bank root, so it can no more carry a separator
    /// or a `..` than an identity can.
    #[test]
    fn an_unsafe_bank_override_is_dropped_not_joined() {
        let dir = tempfile::tempdir().expect("tempdir");
        let b = LocalBank::new(dir.path().join(DEFAULT_BANKS_SUBDIR), "agent-")
            .with_bank_overrides([("alice", "../../escape"), ("bob", "Not/Safe")]);
        assert_eq!(b.bank_id("alice"), "agent-alice");
        assert_eq!(b.bank_id("bob"), "agent-bob");
        assert!(
            b.bank_dir("alice")
                .expect("bank dir")
                .ends_with("agent-alice"),
            "an unsafe override must fall back, never escape the root"
        );
    }

    /// A record id is the filename with ONE `.md` stripped. `trim_end_matches`
    /// would strip every trailing repetition, so a hand-dropped `notes.md.md`
    /// would report the id `notes` — and an invalidate naming that id would then
    /// miss the file it came from, or hit a different one.
    #[test]
    fn a_record_id_strips_exactly_one_extension() {
        let dir = tempfile::tempdir().expect("tempdir");
        let b = bank(dir.path());
        b.retain(&record("alice", "STUDIO-1", "1", "real record"))
            .expect("retain");
        let bank_dir = b.bank_dir("alice").expect("bank dir");
        // A human dropped a note in the bank whose stem itself ends in `.md`.
        std::fs::write(
            bank_dir.join("20260829T120000Z-notes.md.md"),
            "---
identity: alice
ticket: STUDIO-1
state: valid
---

STUDIO-1 hand-written
",
        )
        .expect("write");

        let got = b
            .recall("alice", &ticket_query("STUDIO-1"))
            .expect("recall");
        let ids: Vec<&str> = got.facts.iter().map(|f| f.id.as_str()).collect();
        assert!(
            ids.contains(&"20260829T120000Z-notes.md"),
            "the id keeps the inner .md: {ids:?}"
        );
        // And that id round-trips: invalidating it finds the file it came from.
        assert!(
            b.invalidate("alice", "20260829T120000Z-notes.md", "superseded")
                .expect("invalidate"),
            "a reported id must address the record it was reported for"
        );
    }

    /// `truncate_bytes` never splits a UTF-8 character and marks what it dropped.
    #[test]
    fn truncate_bytes_respects_char_boundaries() {
        let s = "ααααααααα"; // 2 bytes each
        let cut = truncate_bytes(s, 10);
        assert!(cut.len() <= 10, "cut = {} bytes", cut.len());
        assert!(cut.ends_with('…'));
        assert_eq!(truncate_bytes("short", 100), "short");
    }
}
