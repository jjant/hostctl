//! Talking to `nix`: command construction and parsing of its output. See
//! DESIGN.md ("Flake resolution and evaluation").

use std::borrow::Cow;
use std::fmt::Write as _;

use serde::{Deserialize, Serialize};

use crate::config::{FlakeHosts, HostConfig, Kind, NixConfig};
use crate::remote::CommandSpec;
use crate::storepath::StorePath;

/// `--extra-experimental-features 'nix-command flakes'`, first on every
/// nix command line.
pub const FEATURES: [&str; 2] = ["--extra-experimental-features", "nix-command flakes"];

/// Options on every evaluation: skip Nix's eval cache (hostctl has its own;
/// avoids SQLite contention between parallel evals), never write
/// `flake.lock`, and stop pure evaluation from re-resolving already-locked
/// fetches inside Nix's per-input fetcher lock (which serializes parallel
/// evals once `tarball-ttl` lapses).
pub const EVAL_OPTIONS: [&str; 5] = [
    "--no-eval-cache",
    "--no-write-lock-file",
    "--option",
    "tarball-ttl",
    "4294967295",
];

impl NixConfig {
    /// `nix <FEATURES> <words>`.
    fn nix(&self, words: impl IntoIterator<Item = &'static str>) -> CommandSpec {
        CommandSpec::new(&self.command).args(FEATURES).args(words)
    }

    /// `nix <FEATURES> flake metadata --json --no-write-lock-file [--refresh]
    /// <options> <flake>`.
    pub fn metadata(&self, flake: &str, refresh: bool) -> CommandSpec {
        self.nix(["flake", "metadata", "--json", "--no-write-lock-file"])
            .args(refresh.then_some("--refresh"))
            .args(&self.options)
            .arg(flake)
    }

    /// `nix <FEATURES> eval --raw [--read-only] <EVAL_OPTIONS> <options>
    /// '<locked>#.<attr>.outPath'` — the leading `.` makes the attribute
    /// path absolute (no `packages.<system>`/`legacyPackages.<system>`
    /// search, so the answer doesn't depend on where hostctl runs).
    pub fn eval_out_path(&self, locked: &str, attr: &str, read_only: bool) -> CommandSpec {
        self.nix(["eval", "--raw"])
            .args(read_only.then_some("--read-only"))
            .args(EVAL_OPTIONS)
            .args(&self.options)
            .arg(format!("{locked}#.{attr}.outPath"))
    }

    /// `nix <FEATURES> eval --json --read-only <EVAL_OPTIONS> <options>
    /// '<locked>#.' --apply <hosts_expr(inventory)>`: discovery and
    /// inventory in one pure evaluation.
    pub fn hosts(&self, locked: &str, inventory: &str) -> CommandSpec {
        self.nix(["eval", "--json", "--read-only"])
            .args(EVAL_OPTIONS)
            .args(&self.options)
            .arg(format!("{locked}#."))
            .arg("--apply")
            .arg(hosts_expr(inventory))
    }

    /// `nix --version`.
    pub fn version(&self) -> CommandSpec {
        CommandSpec::new(&self.command).arg("--version")
    }
}

/// The `--apply` function for [`NixConfig::hosts`]: given the flake outputs
/// `f`, returns `{ nixos = [names]; darwin = [names]; home = [names];
/// inventory = <f.<inventory path>.hosts or null>; }` (missing or
/// non-attrset outputs → `[]`; `inventory = ""` → always null). Only
/// attribute names and the inventory are forced. The inventory path is split
/// on `.` and walked with `or`, never interpolated as code: every component
/// must be a Nix identifier (anything else → null) and is emitted quoted, so
/// keywords such as `let` work as attribute names too.
pub fn hosts_expr(inventory: &str) -> String {
    let mut expr = String::from(
        "f: let names = o: if builtins.isAttrs o then builtins.attrNames o else [ ]; in { \
         nixos = names (f.nixosConfigurations or null); \
         darwin = names (f.darwinConfigurations or null); \
         home = names (f.homeConfigurations or null); \
         inventory = ",
    );
    if !inventory.is_empty() && inventory.split('.').all(is_identifier) {
        expr.push('f');
        for component in inventory.split('.') {
            let _ = write!(expr, ".\"{component}\"");
        }
        expr.push_str(".hosts or null");
    } else {
        expr.push_str("null");
    }
    expr.push_str("; }");
    expr
}

/// What [`hosts_expr`] evaluates to.
#[derive(Deserialize)]
struct RawHosts {
    #[serde(default)]
    nixos: Vec<String>,
    #[serde(default)]
    darwin: Vec<String>,
    #[serde(default)]
    home: Vec<String>,
    #[serde(default)]
    inventory: serde_json::Value,
}

/// Parse [`NixConfig::hosts`] output. Duplicate names across outputs are
/// kept (resolution decides); `discovered` lists nixos, then darwin, then
/// home-manager names. Inventory entries must match
/// [`crate::config::HostConfig`] (unknown keys are errors naming the host).
pub fn parse_hosts(json: &str) -> Result<FlakeHosts, String> {
    let raw: RawHosts = serde_json::from_str(json)
        .map_err(|e| format!("unexpected output from the hosts evaluation: {e}"))?;
    let discovered = [
        (raw.nixos, Kind::Nixos),
        (raw.darwin, Kind::Darwin),
        (raw.home, Kind::HomeManager),
    ]
    .into_iter()
    .flat_map(|(names, kind)| names.into_iter().map(move |name| (name, kind)))
    .collect();
    let entries = match raw.inventory {
        serde_json::Value::Null => serde_json::Map::new(),
        serde_json::Value::Object(entries) => entries,
        other => {
            return Err(format!(
                "the inventory's `hosts` must be an attribute set of hosts, not {}",
                json_type(&other)
            ));
        }
    };
    let inventory = entries
        .into_iter()
        .map(|(name, value)| match HostConfig::deserialize(value) {
            Ok(host) => Ok((name, host)),
            Err(e) => Err(format!("inventory host {name:?}: {e}")),
        })
        .collect::<Result<_, _>>()?;
    Ok(FlakeHosts {
        discovered,
        inventory,
    })
}

fn json_type(value: &serde_json::Value) -> &'static str {
    match value {
        serde_json::Value::Null => "null",
        serde_json::Value::Bool(_) => "a boolean",
        serde_json::Value::Number(_) => "a number",
        serde_json::Value::String(_) => "a string",
        serde_json::Value::Array(_) => "a list",
        serde_json::Value::Object(_) => "an attribute set",
    }
}

/// A resolved flake, from `nix flake metadata --json`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FlakeInfo {
    /// The flake as configured (`github:jjant/nix-config`).
    pub flake: String,
    /// What to evaluate and what `{flake}` expands to: the metadata's `url`
    /// (locked, e.g. `github:o/r/<rev>?narHash=…`; unlocked `git+file://…`
    /// for dirty trees).
    pub locked: String,
    /// `locked.type` (`github`, `git`, `path`, `tarball`, …).
    pub kind: Option<String>,
    /// `locked.url` (set for `git`/`tarball`/`hg` types; tells local from
    /// remote).
    pub locked_url: Option<String>,
    /// `revision`.
    pub rev: Option<String>,
    /// `dirtyRevision` (`<rev>-dirty`).
    pub dirty_rev: Option<String>,
    /// `lastModified` (unix seconds; commit time for git-based flakes).
    pub last_modified: Option<i64>,
    /// `locked.revCount`.
    pub rev_count: Option<u64>,
    /// `fingerprint` (absent on Lix).
    pub fingerprint: Option<String>,
    /// `locked.narHash`.
    pub nar_hash: Option<String>,
    pub description: Option<String>,
    /// Nix couldn't refresh and fell back to a cached resolution, or hostctl
    /// is using its last good resolution; the reason.
    pub stale: Option<String>,
    /// `flake.lock` is out of date (Nix evaluated with an in-memory lock).
    pub lock_outdated: bool,
}

impl FlakeInfo {
    /// First 7 characters of `rev`, `<7>-dirty` for dirty trees, else `unknown`.
    pub fn short_rev(&self) -> String {
        match (&self.rev, &self.dirty_rev) {
            (Some(rev), _) => rev.chars().take(7).collect(),
            (None, Some(dirty)) => {
                let base = dirty.strip_suffix("-dirty").unwrap_or(dirty);
                format!("{}-dirty", base.chars().take(7).collect::<String>())
            }
            (None, None) => "unknown".into(),
        }
    }

    pub fn is_dirty(&self) -> bool {
        self.dirty_rev.is_some()
    }

    /// Whether another machine can fetch `locked` (github/gitlab/sourcehut,
    /// git/tarball/hg over http(s)/ssh) — false for `path`, `git+file`,
    /// `file`, and dirty trees.
    pub fn fetchable_remotely(&self) -> bool {
        if self.is_dirty() {
            return false;
        }
        match self.kind.as_deref() {
            Some("github" | "gitlab" | "sourcehut") => true,
            Some("git" | "tarball" | "hg") => self.locked_url.as_deref().is_some_and(|u| {
                ["https://", "http://", "ssh://", "git://"]
                    .iter()
                    .any(|scheme| u.starts_with(scheme))
                    || (!u.contains("://") && u.contains(':') && !u.starts_with('/'))
            }),
            _ => false,
        }
    }

    /// The content identity used in cache keys: `fingerprint`, else
    /// narHash + rev + lastModified + revCount + the locked URL (a subflake,
    /// `?dir=…`, shares the others with its repository); `None` when the
    /// flake can't be cached (dirty tree, `lock_outdated`, no narHash either).
    pub fn identity(&self) -> Option<String> {
        if self.is_dirty() || self.lock_outdated {
            return None;
        }
        if let Some(fingerprint) = self.fingerprint.as_deref().filter(|f| !f.is_empty()) {
            return Some(format!("fingerprint={fingerprint}"));
        }
        let nar_hash = self.nar_hash.as_deref().filter(|h| !h.is_empty())?;
        let mut id = format!(
            "narHash={nar_hash};rev={}",
            self.rev.as_deref().unwrap_or("")
        );
        let _ = write!(id, ";lastModified=");
        if let Some(t) = self.last_modified {
            let _ = write!(id, "{t}");
        }
        let _ = write!(id, ";revCount=");
        if let Some(n) = self.rev_count {
            let _ = write!(id, "{n}");
        }
        let _ = write!(id, ";url={}", self.locked);
        Some(id)
    }

    /// For github/gitlab/sourcehut flakes (honoring `?host=`), a web URL
    /// comparing `from` with this revision
    /// (`https://github.com/o/r/compare/<from>...<rev>`; sourcehut has no
    /// compare view, so its log at this revision). `None` unless both
    /// revisions are hex (a `-dirty` suffix on `from` is ignored).
    pub fn compare_url(&self, from: &str) -> Option<String> {
        let kind = self.kind.as_deref()?;
        let default_host = match kind {
            "github" => "github.com",
            "gitlab" => "gitlab.com",
            "sourcehut" => "git.sr.ht",
            _ => return None,
        };
        let rev = self.rev.as_deref().filter(|r| is_hex(r))?;
        let from = from.strip_suffix("-dirty").unwrap_or(from);
        if !is_hex(from) {
            return None;
        }
        // `<kind>:<owner>/<repo>/<rev>?<query>`; gitlab subgroups are `%2F`-escaped.
        let rest = self.locked.strip_prefix(kind)?.strip_prefix(':')?;
        let (path, query) = rest.split_once('?').unwrap_or((rest, ""));
        let mut parts = path.split('/');
        let (owner, repo) = (parts.next()?, parts.next()?);
        if owner.is_empty() || repo.is_empty() {
            return None;
        }
        let host = query
            .split('&')
            .find_map(|param| param.strip_prefix("host="))
            .filter(|h| !h.is_empty())
            .map_or(Cow::Borrowed(default_host), percent_decode);
        let base = format!(
            "https://{host}/{}/{}",
            percent_decode(owner),
            percent_decode(repo)
        );
        Some(match kind {
            "github" => format!("{base}/compare/{from}...{rev}"),
            "gitlab" => format!("{base}/-/compare/{from}...{rev}"),
            _ => format!("{base}/log/{rev}"),
        })
    }
}

fn is_hex(s: &str) -> bool {
    !s.is_empty() && s.len() <= 64 && s.bytes().all(|b| b.is_ascii_hexdigit())
}

/// Decode `%XX` escapes; text that wouldn't decode to UTF-8 is kept as is.
fn percent_decode(s: &str) -> Cow<'_, str> {
    if !s.contains('%') {
        return Cow::Borrowed(s);
    }
    let hex = |b: &u8| char::from(*b).to_digit(16);
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        let escaped = if bytes[i] == b'%' {
            let digits = bytes.get(i + 1).zip(bytes.get(i + 2));
            digits.and_then(|(high, low)| Some(hex(high)? * 16 + hex(low)?))
        } else {
            None
        };
        match escaped {
            Some(byte) => {
                out.push(byte as u8);
                i += 3;
            }
            None => {
                out.push(bytes[i]);
                i += 1;
            }
        }
    }
    String::from_utf8(out).map_or(Cow::Borrowed(s), Cow::Owned)
}

/// The fields of `nix flake metadata --json` hostctl uses.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawMetadata {
    url: Option<String>,
    description: Option<String>,
    revision: Option<String>,
    dirty_revision: Option<String>,
    last_modified: Option<i64>,
    rev_count: Option<u64>,
    fingerprint: Option<String>,
    #[serde(default)]
    locked: RawLocked,
}

#[derive(Default, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawLocked {
    #[serde(rename = "type")]
    kind: Option<String>,
    url: Option<String>,
    rev: Option<String>,
    dirty_rev: Option<String>,
    last_modified: Option<i64>,
    rev_count: Option<u64>,
    nar_hash: Option<String>,
}

/// Lock-file notices: the lock Nix evaluated with differs from (or isn't)
/// the flake's `flake.lock`.
const LOCK_NOTICES: [&str; 5] = [
    "not writing modified lock file",
    "creating lock file",
    "updating lock file",
    "cannot write modified lock file",
    "will not write lock file",
];

/// Parse `nix flake metadata --json` stdout (requires `url`; everything else
/// optional, Nix/Lix/Determinate differ; top-level and `locked.*` fields
/// back each other up) and inspect its `stderr`: a download that fell back
/// to Nix's cache (`; using cached version`) or failed without a retry
/// (`unable to download`) → `stale`, with the warning as the reason;
/// `not writing modified lock file` / `creating lock file` (and the other
/// lock-file notices) → `lock_outdated`. See the fixtures in
/// `tests/fixtures/` for real outputs (github, path, git clean and dirty).
pub fn parse_metadata(flake: &str, stdout: &str, stderr: &str) -> Result<FlakeInfo, String> {
    if stdout.trim().is_empty() {
        return Err("nix flake metadata printed nothing".into());
    }
    let raw: RawMetadata = serde_json::from_str(stdout)
        .map_err(|e| format!("unexpected output from nix flake metadata: {e}"))?;
    let locked = raw
        .url
        .filter(|url| !url.is_empty())
        .ok_or("nix flake metadata printed no locked url")?;
    let stderr = strip_ansi(stderr);
    Ok(FlakeInfo {
        flake: flake.to_string(),
        locked,
        kind: raw.locked.kind,
        locked_url: raw.locked.url,
        rev: raw.revision.or(raw.locked.rev),
        dirty_rev: raw.dirty_revision.or(raw.locked.dirty_rev),
        last_modified: raw.last_modified.or(raw.locked.last_modified),
        rev_count: raw.locked.rev_count.or(raw.rev_count),
        fingerprint: raw.fingerprint,
        nar_hash: raw.locked.nar_hash,
        description: raw.description,
        stale: stale_reason(&stderr),
        lock_outdated: LOCK_NOTICES.iter().any(|notice| stderr.contains(notice)),
    })
}

/// The first warning saying a download fell back to a cached copy or
/// failed for good — a successful retry (`…; retrying in 57 ms`) is fine.
/// A message runs from its `warning:`/`error` line to the next one, since
/// an HTTP error's response body continues it unindented.
fn stale_reason(stderr: &str) -> Option<String> {
    struct Message<'a> {
        header: &'a str,
        cached: bool,
        retrying: bool,
        rate_limited: bool,
    }

    impl Message<'_> {
        fn stale_reason(&self) -> Option<String> {
            // Errors (`error (ignored): …`) aren't fallbacks.
            let text = self.header.strip_prefix("warning:")?.trim_start();
            let failed = text.contains("unable to download") && !self.retrying;
            if !self.cached && !failed {
                return None;
            }
            let text = text.strip_prefix("error:").unwrap_or(text);
            let text = text.split("; using cached version").next().unwrap_or(text);
            let mut reason = text.trim().trim_end_matches(';').to_string();
            if reason.is_empty() {
                reason = "using a cached copy".into();
            }
            if self.rate_limited && !reason.contains("rate limit") {
                reason.push_str(" (API rate limit exceeded)");
            }
            Some(reason)
        }
    }

    let mut message: Option<Message<'_>> = None;
    for line in stderr.lines() {
        if line.starts_with("warning:") || line.starts_with("error") {
            if let Some(reason) = message.take().and_then(|m| m.stale_reason()) {
                return Some(reason);
            }
            message = Some(Message {
                header: line,
                cached: false,
                retrying: false,
                rate_limited: false,
            });
        }
        if let Some(m) = message.as_mut() {
            m.cached |= line.contains("using cached version");
            m.retrying |= line.contains("; retrying in");
            m.rate_limited |= line.contains("rate limit");
        }
    }
    message.and_then(|m| m.stale_reason())
}

/// Parse `nix eval --raw …outPath` output (trailing whitespace allowed).
pub fn parse_out_path(stdout: &str) -> Result<StorePath, String> {
    let out = stdout.trim_end();
    if out.is_empty() {
        return Err("nix eval printed nothing".into());
    }
    // `StorePath::parse` takes whatever precedes the last `/` as the store
    // directory, so a second line would hide behind it.
    let one_word = !out.contains(|c: char| c.is_whitespace() || c.is_control());
    match StorePath::parse(out) {
        Some(path) if one_word => Ok(path),
        _ => Err(format!(
            "nix eval printed {:?}, not a store path",
            crate::util::truncate(out, 200)
        )),
    }
}

/// The first line of `nix --version`, trimmed (`nix (Nix) 2.35.3`,
/// `nix (Determinate Nix 3.8.2) 2.30.1`, `nix (Lix, like Nix) 2.93.0`) —
/// the whole line, because implementations with equal version numbers
/// evaluate differently. `None` if empty.
pub fn parse_version(stdout: &str) -> Option<String> {
    stdout
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .map(str::to_string)
}

/// Whether a failed `--read-only` evaluation should be retried normally:
/// whatever needs the store written fails in read-only mode with `error:
/// path '/nix/store/…' is not valid` — import-from-derivation (a `….drv`),
/// `builtins.toFile`, `"${./file}"`.
pub fn is_read_only_failure(stderr: &str) -> bool {
    const START: &str = "path '";
    stderr.match_indices(START).any(|(i, _)| {
        stderr[i + START.len()..]
            .split_once('\'')
            .is_some_and(|(path, rest)| {
                // Nix 2.35 prints the full path, 2.34 just `<hash>-<name>`.
                let store_path = path.starts_with('/')
                    || (path.len() > 33
                        && path.as_bytes()[32] == b'-'
                        && path[..32].bytes().all(|b| b.is_ascii_alphanumeric()));
                store_path && !path.contains('\n') && rest.starts_with(" is not valid")
            })
    })
}

/// Reduce nix's stderr to what a user needs to see: drop noise (`warning:
/// Git tree '…' is dirty`, `Using saved setting for …`, `waiting for another
/// Nix process to finish …`, `error (ignored): …`, `copying path …`,
/// `unpacking '…' into the Git cache…`, `building '…'`, the lock-file
/// notices with their `• …` lines, the two-line untrusted `nixConfig`
/// warning, `these N paths will be fetched` / `… will be built` with their
/// path lists, ANSI escapes, blank lines at the ends), keep everything else
/// in order (in particular the whole final `error:` block with its indented
/// trace). A message's indented, blank and `• ` lines go with it.
pub fn clean_stderr(stderr: &str) -> String {
    let stderr = strip_ansi(stderr);
    let mut kept = Vec::new();
    let mut dropping = false;
    for line in stderr.lines() {
        if !is_continuation(line) {
            dropping = is_noise(line);
        }
        if !dropping {
            kept.push(line);
        }
    }
    let blank = |line: &&str| line.trim().is_empty();
    let start = kept.iter().position(|l| !blank(l)).unwrap_or(kept.len());
    let end = kept
        .iter()
        .rposition(|l| !blank(l))
        .map_or(start, |i| i + 1);
    kept[start..end].join("\n")
}

fn is_continuation(line: &str) -> bool {
    line.is_empty()
        || line.starts_with([' ', '\t'])
        || line.starts_with("• ")
        || line.starts_with("Pass '--accept-flake-config'")
}

fn is_noise(line: &str) -> bool {
    const NOISE: [&str; 10] = [
        "warning: Git tree '",
        "warning: you don't have Internet access",
        "Using saved setting for ",
        "waiting for ",
        "error (ignored): ",
        "copying path ",
        "copying '",
        "unpacking '",
        "building '",
        "warning: ignoring untrusted flake configuration setting ",
    ];
    NOISE.iter().any(|prefix| line.starts_with(prefix))
        || line
            .strip_prefix("warning: ")
            .is_some_and(|w| LOCK_NOTICES.iter().any(|notice| w.starts_with(notice)))
        || ((line.starts_with("these ") || line.starts_with("this "))
            && (line.contains(" will be fetched") || line.contains(" will be built")))
}

/// Remove ANSI CSI sequences (colors), in case a wrapper forces them.
fn strip_ansi(s: &str) -> Cow<'_, str> {
    if !s.contains('\x1b') {
        return Cow::Borrowed(s);
    }
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        if c != '\x1b' {
            out.push(c);
        } else if chars.next() == Some('[') {
            // Parameters and intermediates up to the final byte (@ to ~).
            for c in chars.by_ref() {
                if ('@'..='~').contains(&c) {
                    break;
                }
            }
        }
    }
    Cow::Owned(out)
}

/// One line describing a nix failure: the text after the last line-leading
/// `error: ` (nix's innermost message, e.g. `attribute 'bigmem' missing`;
/// source excerpts in traces don't count), else the last non-empty line of
/// [`clean_stderr`] (a bare `error:` header doesn't count), else `fallback`.
/// Trailing `:`/`;` are dropped.
pub fn error_summary(stderr: &str, fallback: &str) -> String {
    let cleaned = clean_stderr(stderr);
    let innermost = cleaned.lines().rev().find_map(|line| {
        let message = line.trim_start().strip_prefix("error:")?.trim();
        (!message.is_empty()).then_some(message)
    });
    let line = innermost.or_else(|| {
        cleaned
            .lines()
            .rev()
            .map(str::trim)
            .find(|l| !l.is_empty() && *l != "error:")
    });
    match line.map(|l| l.trim_end_matches([':', ';']).trim_end()) {
        Some(summary) if !summary.is_empty() => summary.to_string(),
        _ => fallback.to_string(),
    }
}

/// `[A-Za-z_][A-Za-z0-9_'-]*`.
fn is_identifier(s: &str) -> bool {
    let mut bytes = s.bytes();
    matches!(bytes.next(), Some(b) if b.is_ascii_alphabetic() || b == b'_')
        && bytes.all(|b| b.is_ascii_alphanumeric() || b"_'-".contains(&b))
}

/// Quote one attribute name for an installable's attribute path when needed:
/// identifiers (`[A-Za-z_][A-Za-z0-9_'-]*`) stay as they are; anything else
/// becomes `"…"` with every byte outside `[A-Za-z0-9-._~!$&'()*+,;=:@/?]`
/// percent-encoded — installable fragments are percent-decoded (so `"pct%41"`
/// would mean `pctA`), `^` would start an outputs spec, and spaces,
/// non-ASCII, `\`, `{`, … don't parse as a URL. A name containing `"` can't
/// be expressed at all (Nix has no escape inside a quoted attribute name).
pub fn attr_name(name: &str) -> Cow<'_, str> {
    if is_identifier(name) {
        return Cow::Borrowed(name);
    }
    let mut out = String::with_capacity(name.len() + 2);
    out.push('"');
    for b in name.bytes() {
        if b.is_ascii_alphanumeric() || b"-._~!$&'()*+,;=:@/?".contains(&b) {
            out.push(char::from(b));
        } else {
            let _ = write!(out, "%{b:02X}");
        }
    }
    out.push('"');
    Cow::Owned(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    const GITHUB: &str = include_str!("../tests/fixtures/metadata-github.json");
    const PATH: &str = include_str!("../tests/fixtures/metadata-path.json");
    const GIT: &str = include_str!("../tests/fixtures/metadata-git.json");
    const GIT_DIRTY: &str = include_str!("../tests/fixtures/metadata-git-dirty.json");
    const GIT_DIRTY_STDERR: &str = include_str!("../tests/fixtures/metadata-git-dirty.stderr");
    const IFD_STDERR: &str = include_str!("../tests/fixtures/eval-ifd-read-only.stderr");
    const MISSING_ATTR_STDERR: &str = include_str!("../tests/fixtures/eval-missing-attr.stderr");
    const NOISE_STDERR: &str = include_str!("../tests/fixtures/eval-noise.stderr");
    const PARALLEL_NOISE_STDERR: &str =
        include_str!("../tests/fixtures/eval-parallel-noise.stderr");
    const NIX_VERSION: &str = include_str!("../tests/fixtures/nix-version.txt");

    // Captured from nix 2.35.3 on bigmem.
    /// `flake metadata --refresh github:…` with the network down: a retry,
    /// then the fallback.
    const STALE_STDERR: &str = "\
warning: unable to download 'https://api.github.com/repos/jjant/nix-config/commits/HEAD': Could not connect to server (7) Failed to connect to api.github.com port 443 via 127.0.0.1 after 0 ms: Could not connect to server; retrying in 57 ms (attempt 1/2)
warning: unable to download 'https://api.github.com/repos/jjant/nix-config/commits/HEAD': Could not connect to server (7) Failed to connect to api.github.com port 443 via 127.0.0.1 after 0 ms: Could not connect to server; using cached version
";
    /// The same for a repository Nix has never fetched (exit 1).
    const FETCH_FAILED_STDERR: &str = "\
error:
       … while fetching the input 'github:jjant/does-not-exist-hostctl'

       error: unable to download 'https://api.github.com/repos/jjant/does-not-exist-hostctl/commits/HEAD': Could not connect to server (7) Failed to connect to api.github.com port 443 via 127.0.0.1 after 0 ms: Could not connect to server
";
    /// `flake metadata --no-write-lock-file` of a flake without `flake.lock`.
    const LOCK_STDERR: &str = "\
warning: not writing modified lock file of flake 'git+file:///tmp/hostctl-exp/flake':
• Added input 'dep':
    'path:/tmp/hostctl-exp/dep?lastModified=1792246836&narHash=sha256-m/i4zIJHKRtapNnPwNUoFromRkwfucNCdbqZ1oxSjWo%3D' (2026-10-17)
";
    /// The same without `--no-write-lock-file`.
    const CREATING_LOCK_STDERR: &str = "\
warning: creating lock file \"/tmp/hostctl-exp/withinp/flake/flake.lock\":
• Added input 'dep':
    'path:/tmp/hostctl-exp/withinp/dep?lastModified=1792247136&narHash=sha256-Kb/pCu9YKNiGv2MsO32O/W9ohYyo3g7ZeWFChSp3GXU%3D' (2026-10-17)
";
    /// A flake with `nixConfig`, evaluated without `--accept-flake-config`.
    const UNTRUSTED_STDERR: &str = "\
warning: ignoring untrusted flake configuration setting 'extra-substituters'.
Pass '--accept-flake-config' to trust it
warning: ignoring untrusted flake configuration setting 'extra-trusted-public-keys'.
Pass '--accept-flake-config' to trust it
";
    /// `builtins.readFile (builtins.toFile "x" "y")` under `--read-only`.
    const TOFILE_READ_ONLY_STDERR: &str = "\
error:
       … while calling the 'readFile' builtin
         at «git+file:///tmp/hostctl-exp/flake?ref=refs/heads/main&rev=205ce5ed221b57fdd871db9390ed46fb51712b7e»/flake.nix:6:14:
            5|     ifd = import drv;
            6|     tofile = builtins.readFile (builtins.toFile \"x\" \"y\");
             |              ^
            7|     interp = builtins.readFile \"${./file}\";

       … while realising the context of path '/nix/store/lfngsssysp6h1v4ccqg23c52s9sjl779-x'

       error: path '/nix/store/lfngsssysp6h1v4ccqg23c52s9sjl779-x' is not valid
";

    const HM: &str = "/nix/store/ykn4vz9vpqccp1n36p50irs3dwrjfkk6-home-manager-generation";
    const LOCKED: &str = "github:jjant/nix-config/42ea902131b93c04d098d3bcd02f29305b4bad7c?narHash=sha256-rTwky4CPWade4oeZ81h5PcDt8%2Be8kXznGgaiDqYFhFk%3D";

    fn strings(words: &[&str]) -> Vec<String> {
        words.iter().map(|w| w.to_string()).collect()
    }

    fn with_options(options: &[&str]) -> NixConfig {
        NixConfig {
            command: "/run/current-system/sw/bin/nix".into(),
            options: strings(options),
            ..NixConfig::default()
        }
    }

    fn info(json: &str) -> FlakeInfo {
        parse_metadata("f", json, "").unwrap()
    }

    #[test]
    fn metadata_command_line() {
        let spec = NixConfig::default().metadata("github:jjant/nix-config", false);
        assert_eq!(spec.program, "nix");
        assert_eq!(
            spec.args,
            strings(&[
                "--extra-experimental-features",
                "nix-command flakes",
                "flake",
                "metadata",
                "--json",
                "--no-write-lock-file",
                "github:jjant/nix-config",
            ])
        );
        assert_eq!((spec.stdin, spec.env), (None, vec![]));

        let spec = with_options(&["--store", "local?root=/tmp/s"]).metadata(".", true);
        assert_eq!(spec.program, "/run/current-system/sw/bin/nix");
        assert_eq!(
            spec.args,
            strings(&[
                "--extra-experimental-features",
                "nix-command flakes",
                "flake",
                "metadata",
                "--json",
                "--no-write-lock-file",
                "--refresh",
                "--store",
                "local?root=/tmp/s",
                ".",
            ])
        );
    }

    #[test]
    fn eval_command_line() {
        let attr = "homeConfigurations.\"jj@mac\".activationPackage";
        let spec = NixConfig::default().eval_out_path(LOCKED, attr, true);
        assert_eq!(spec.program, "nix");
        assert_eq!(
            spec.args,
            strings(&[
                "--extra-experimental-features",
                "nix-command flakes",
                "eval",
                "--raw",
                "--read-only",
                "--no-eval-cache",
                "--no-write-lock-file",
                "--option",
                "tarball-ttl",
                "4294967295",
                &format!("{LOCKED}#.homeConfigurations.\"jj@mac\".activationPackage.outPath"),
            ])
        );
        assert_eq!((spec.stdin, spec.env), (None, vec![]));

        let spec = with_options(&["--impure", "-L"]).eval_out_path("path:/f", "x.y", false);
        assert_eq!(
            spec.args,
            strings(&[
                "--extra-experimental-features",
                "nix-command flakes",
                "eval",
                "--raw",
                "--no-eval-cache",
                "--no-write-lock-file",
                "--option",
                "tarball-ttl",
                "4294967295",
                "--impure",
                "-L",
                "path:/f#.x.y.outPath",
            ])
        );
    }

    #[test]
    fn hosts_command_line() {
        let spec = with_options(&["--offline"]).hosts(LOCKED, "lib.hostctl");
        assert_eq!(spec.program, "/run/current-system/sw/bin/nix");
        assert_eq!(
            spec.args,
            strings(&[
                "--extra-experimental-features",
                "nix-command flakes",
                "eval",
                "--json",
                "--read-only",
                "--no-eval-cache",
                "--no-write-lock-file",
                "--option",
                "tarball-ttl",
                "4294967295",
                "--offline",
                &format!("{LOCKED}#."),
                "--apply",
                &hosts_expr("lib.hostctl"),
            ])
        );
        assert_eq!((spec.stdin, spec.env), (None, vec![]));
    }

    #[test]
    fn version_command_line() {
        let spec = with_options(&["--offline"]).version();
        assert_eq!(spec.program, "/run/current-system/sw/bin/nix");
        assert_eq!(spec.args, strings(&["--version"]));
    }

    #[test]
    fn hosts_expression_walks_the_inventory_path() {
        assert_eq!(
            hosts_expr("lib.hostctl"),
            "f: let names = o: if builtins.isAttrs o then builtins.attrNames o else [ ]; in { \
             nixos = names (f.nixosConfigurations or null); \
             darwin = names (f.darwinConfigurations or null); \
             home = names (f.homeConfigurations or null); \
             inventory = f.\"lib\".\"hostctl\".hosts or null; }"
        );
        assert!(hosts_expr("fleet").ends_with("inventory = f.\"fleet\".hosts or null; }"));
        // Keywords are fine as quoted attribute names.
        assert!(hosts_expr("let.in.or").contains("f.\"let\".\"in\".\"or\".hosts or null"));
        assert!(hosts_expr("a_b.c'd.e-f").contains("f.\"a_b\".\"c'd\".\"e-f\".hosts or null"));
    }

    #[test]
    fn hosts_expression_never_interpolates_bad_paths() {
        for bad in [
            "",
            ".",
            "lib.",
            ".lib",
            "lib..hostctl",
            "${builtins.abort 1}",
            "lib.${builtins.abort \"x\"}",
            "lib.hostctl\" or builtins.abort \"x",
            "lib.host ctl",
            "1lib",
            "lib.-x",
            "lib.\"hostctl\"",
            "lib/hostctl",
            "lib.hostctl;",
            "lïb",
        ] {
            let expr = hosts_expr(bad);
            assert!(expr.ends_with("inventory = null; }"), "{bad:?}: {expr}");
            assert!(!expr.contains("abort"), "{bad:?}");
        }
    }

    #[test]
    fn parses_hosts() {
        let json = r#"{"darwin":["mac-m1"],"home":["bigmem","jj@mac"],"inventory":{"web1":{"destination":"root@web1.lan","note":"x","optional":true,"kind":"nixos","activate":false,"hostnames":["web"],"ssh-options":["-p","2222"],"profile":null}},"nixos":["bigmem","web1"]}"#;
        let hosts = parse_hosts(json).unwrap();
        assert_eq!(
            hosts.discovered,
            vec![
                ("bigmem".to_string(), Kind::Nixos),
                ("web1".to_string(), Kind::Nixos),
                ("mac-m1".to_string(), Kind::Darwin),
                ("bigmem".to_string(), Kind::HomeManager),
                ("jj@mac".to_string(), Kind::HomeManager),
            ]
        );
        let web1 = &hosts.inventory["web1"];
        assert_eq!(web1.destination.as_deref(), Some("root@web1.lan"));
        assert_eq!(web1.note.as_deref(), Some("x"));
        assert_eq!(web1.optional, Some(true));
        assert_eq!(web1.kind, Some(Kind::Nixos));
        assert_eq!(
            web1.activate,
            Some(crate::config::ActivateSetting::Enabled(false))
        );
        assert_eq!(web1.hostnames, Some(vec!["web".to_string()]));
        assert_eq!(web1.ssh_options, Some(strings(&["-p", "2222"])));
        assert_eq!(web1.profile, None, "null means unset");
        assert_eq!(hosts.inventory.len(), 1);
    }

    #[test]
    fn parses_hosts_without_inventory() {
        let empty = parse_hosts(r#"{"darwin":[],"home":[],"inventory":null,"nixos":[]}"#).unwrap();
        assert_eq!(empty, FlakeHosts::default());
        let sparse = parse_hosts(r#"{"nixos":["a"]}"#).unwrap();
        assert_eq!(sparse.discovered, vec![("a".to_string(), Kind::Nixos)]);
        assert!(sparse.inventory.is_empty());
        let unicode = parse_hosts(r#"{"home":["ü^x"],"inventory":{}}"#).unwrap();
        assert_eq!(
            unicode.discovered,
            vec![("ü^x".to_string(), Kind::HomeManager)]
        );
    }

    #[test]
    fn inventory_errors_name_the_host() {
        let err = parse_hosts(r#"{"inventory":{"web1":{"destination":"x","colour":"red"}}}"#)
            .unwrap_err();
        assert!(err.contains("\"web1\""), "{err}");
        assert!(err.contains("colour"), "{err}");
        let err = parse_hosts(r#"{"inventory":{"web2":{"optional":"yes"}}}"#).unwrap_err();
        assert!(
            err.contains("\"web2\"") && err.contains("invalid type"),
            "{err}"
        );
        let err = parse_hosts(r#"{"inventory":{"web3":"root@web3"}}"#).unwrap_err();
        assert!(err.contains("\"web3\""), "{err}");
        let err = parse_hosts(r#"{"inventory":["web1"]}"#).unwrap_err();
        assert!(err.contains("not a list"), "{err}");
        let err = parse_hosts(r#"{"inventory":"lib"}"#).unwrap_err();
        assert!(err.contains("not a string"), "{err}");
        assert!(parse_hosts("").is_err());
        assert!(parse_hosts("null").is_err());
        assert!(parse_hosts(r#"{"nixos":[1]}"#).is_err());
        assert!(parse_hosts(r#"{"home":"bigmem"}"#).is_err());
    }

    #[test]
    fn parses_github_metadata() {
        let info = parse_metadata("github:jjant/nix-config", GITHUB, "").unwrap();
        assert_eq!(
            info,
            FlakeInfo {
                flake: "github:jjant/nix-config".into(),
                locked: LOCKED.into(),
                kind: Some("github".into()),
                locked_url: None,
                rev: Some("42ea902131b93c04d098d3bcd02f29305b4bad7c".into()),
                dirty_rev: None,
                last_modified: Some(1792240011),
                rev_count: None,
                fingerprint: Some(
                    "773b1c0a8f4cbe4ff5c09fd6c963dc4df2680450367f592cf85c58dd280bfc74".into()
                ),
                nar_hash: Some("sha256-rTwky4CPWade4oeZ81h5PcDt8+e8kXznGgaiDqYFhFk=".into()),
                description: Some("jjant's nix config".into()),
                stale: None,
                lock_outdated: false,
            }
        );
        assert_eq!(info.short_rev(), "42ea902");
        assert!(info.fetchable_remotely());
    }

    #[test]
    fn parses_path_metadata() {
        let info = parse_metadata("/tmp/hostctl-ifd", PATH, "").unwrap();
        assert_eq!(
            info.locked,
            "path:/tmp/hostctl-ifd?lastModified=1792240484&narHash=sha256-uS%2BcPlNAUea9SZ0ceqioQrf36oZQIcJCt8QBY8G%2B5/s%3D"
        );
        assert_eq!(info.kind.as_deref(), Some("path"));
        assert_eq!(info.locked_url, None);
        assert_eq!(
            (&info.rev, &info.dirty_rev, info.rev_count),
            (&None, &None, None)
        );
        assert_eq!(info.last_modified, Some(1792240484));
        assert_eq!(
            info.nar_hash.as_deref(),
            Some("sha256-uS+cPlNAUea9SZ0ceqioQrf36oZQIcJCt8QBY8G+5/s=")
        );
        assert!(info.fingerprint.is_some());
        assert_eq!(info.description, None);
        assert!(!info.fetchable_remotely());
    }

    #[test]
    fn parses_git_metadata() {
        let info = parse_metadata("git+file:///tmp/hostctl-ifd", GIT, "").unwrap();
        assert_eq!(
            info.locked,
            "git+file:///tmp/hostctl-ifd?ref=refs/heads/main&rev=a8f62b6f2c1170af6c40178c9080c87fc51fe5de"
        );
        assert_eq!(info.kind.as_deref(), Some("git"));
        assert_eq!(info.locked_url.as_deref(), Some("file:///tmp/hostctl-ifd"));
        assert_eq!(
            info.rev.as_deref(),
            Some("a8f62b6f2c1170af6c40178c9080c87fc51fe5de")
        );
        assert_eq!(info.rev_count, Some(1));
        assert_eq!(info.last_modified, Some(1792240480));
        assert!(!info.is_dirty() && !info.lock_outdated && info.stale.is_none());
        assert!(!info.fetchable_remotely(), "git+file is local");
        assert!(info.identity().is_some());
    }

    #[test]
    fn parses_dirty_git_metadata() {
        let info = parse_metadata(".", GIT_DIRTY, GIT_DIRTY_STDERR).unwrap();
        assert_eq!(info.locked, "git+file:///tmp/hostctl-ifd");
        assert_eq!(info.rev, None);
        assert_eq!(
            info.dirty_rev.as_deref(),
            Some("a8f62b6f2c1170af6c40178c9080c87fc51fe5de-dirty")
        );
        assert!(info.is_dirty());
        assert_eq!(info.short_rev(), "a8f62b6-dirty");
        assert_eq!(info.stale, None, "a dirty tree isn't stale");
        assert!(!info.lock_outdated);
        assert!(!info.fetchable_remotely());
        assert_eq!(info.identity(), None);
    }

    /// Lix prints no `fingerprint` (nor `locked.__final`).
    const LIX_GITHUB: &str = r#"{"description":"jjant's nix config","lastModified":1792240011,"locked":{"lastModified":1792240011,"narHash":"sha256-rTwky4CPWade4oeZ81h5PcDt8+e8kXznGgaiDqYFhFk=","owner":"jjant","repo":"nix-config","rev":"42ea902131b93c04d098d3bcd02f29305b4bad7c","type":"github"},"locks":{"nodes":{"root":{}},"root":"root","version":7},"original":{"owner":"jjant","repo":"nix-config","type":"github"},"originalUrl":"github:jjant/nix-config","path":"/nix/store/2m1x68xnval0fzmbns74k00cv05f45dv-source","resolved":{"owner":"jjant","repo":"nix-config","type":"github"},"resolvedUrl":"github:jjant/nix-config","revision":"42ea902131b93c04d098d3bcd02f29305b4bad7c","url":"github:jjant/nix-config/42ea902131b93c04d098d3bcd02f29305b4bad7c?narHash=sha256-rTwky4CPWade4oeZ81h5PcDt8%2Be8kXznGgaiDqYFhFk%3D"}"#;

    #[test]
    fn parses_lix_metadata_without_fingerprint() {
        let info = parse_metadata("github:jjant/nix-config", LIX_GITHUB, "").unwrap();
        assert_eq!(info.fingerprint, None);
        assert_eq!(info.locked, LOCKED);
        assert_eq!(
            info.identity().as_deref(),
            Some(
                "narHash=sha256-rTwky4CPWade4oeZ81h5PcDt8+e8kXznGgaiDqYFhFk=;\
                 rev=42ea902131b93c04d098d3bcd02f29305b4bad7c;lastModified=1792240011;\
                 revCount=;url=github:jjant/nix-config/42ea902131b93c04d098d3bcd02f29305b4bad7c\
                 ?narHash=sha256-rTwky4CPWade4oeZ81h5PcDt8%2Be8kXznGgaiDqYFhFk%3D"
            )
        );
    }

    #[test]
    fn parses_minimal_metadata() {
        let info = parse_metadata("x", r#"{"url":"path:/x"}"#, "").unwrap();
        assert_eq!(
            info,
            FlakeInfo {
                flake: "x".into(),
                locked: "path:/x".into(),
                kind: None,
                locked_url: None,
                rev: None,
                dirty_rev: None,
                last_modified: None,
                rev_count: None,
                fingerprint: None,
                nar_hash: None,
                description: None,
                stale: None,
                lock_outdated: false,
            }
        );
        assert_eq!(info.identity(), None, "nothing identifies its content");
        assert_eq!(info.short_rev(), "unknown");
        // `locked.*` backs up the top-level fields.
        let info = parse_metadata(
            "x",
            r#"{"url":"u","locked":{"rev":"abc","dirtyRev":"def-dirty","lastModified":5,"revCount":3}}"#,
            "",
        )
        .unwrap();
        assert_eq!(info.rev.as_deref(), Some("abc"));
        assert_eq!(info.dirty_rev.as_deref(), Some("def-dirty"));
        assert_eq!((info.last_modified, info.rev_count), (Some(5), Some(3)));
    }

    #[test]
    fn rejects_bad_metadata() {
        for (stdout, want) in [
            ("", "printed nothing"),
            ("  \n", "printed nothing"),
            ("{", "unexpected output"),
            ("[]", "unexpected output"),
            (r#"{"description":"x"}"#, "no locked url"),
            (r#"{"url":""}"#, "no locked url"),
            (
                r#"{"url":"u","lastModified":"yesterday"}"#,
                "unexpected output",
            ),
        ] {
            let err = parse_metadata("f", stdout, "").unwrap_err();
            assert!(err.contains(want), "{stdout:?}: {err}");
        }
    }

    #[test]
    fn detects_stale_resolutions() {
        let info = parse_metadata("github:jjant/nix-config", GITHUB, STALE_STDERR).unwrap();
        assert_eq!(
            info.stale.as_deref(),
            Some(
                "unable to download 'https://api.github.com/repos/jjant/nix-config/commits/HEAD': \
                 Could not connect to server (7) Failed to connect to api.github.com port 443 via \
                 127.0.0.1 after 0 ms: Could not connect to server"
            )
        );
        assert!(!info.lock_outdated);
        assert!(info.identity().is_some(), "stale results are still valid");

        // Older Nix (and Lix) wrap the error, and a 403 brings its body along.
        let rate_limited = "warning: error: unable to download 'https://api.github.com/repos/o/r/commits/HEAD': HTTP error 403\n\nresponse body:\n\n{\"message\":\"API rate limit exceeded for 203.0.113.7.\"}; using cached version\n";
        assert_eq!(
            stale_reason(rate_limited).as_deref(),
            Some(
                "unable to download 'https://api.github.com/repos/o/r/commits/HEAD': HTTP error \
                 403 (API rate limit exceeded)"
            )
        );
        let indented = "warning: unable to download 'https://x/y': HTTP error 403\n\n       response body:\n\n       {\"message\":\"API rate limit exceeded\"}; using cached version";
        assert_eq!(
            stale_reason(indented).as_deref(),
            Some("unable to download 'https://x/y': HTTP error 403 (API rate limit exceeded)")
        );
        let fallback = "warning: unable to download 'https://x/y': timeout\n";
        assert_eq!(
            stale_reason(fallback).as_deref(),
            Some("unable to download 'https://x/y': timeout")
        );
        let cached_only = "warning: some fetcher failed; using cached version\n";
        assert_eq!(
            stale_reason(cached_only).as_deref(),
            Some("some fetcher failed")
        );
        let second = format!("warning: unknown setting 'foo'\n{STALE_STDERR}");
        assert_eq!(stale_reason(&second), stale_reason(STALE_STDERR));
    }

    #[test]
    fn a_successful_retry_is_not_stale() {
        let retried = STALE_STDERR.lines().next().unwrap();
        assert_eq!(stale_reason(retried), None);
        for fine in [
            "",
            GIT_DIRTY_STDERR,
            NOISE_STDERR,
            PARALLEL_NOISE_STDERR,
            LOCK_STDERR,
            UNTRUSTED_STDERR,
            "warning: unknown setting 'foo'\n",
            "error (ignored): unable to download 'https://cache.example.org/x.narinfo': HTTP error 500\n",
        ] {
            assert_eq!(stale_reason(fine), None, "{fine:?}");
        }
    }

    #[test]
    fn detects_outdated_lock_files() {
        for stderr in [
            LOCK_STDERR,
            CREATING_LOCK_STDERR,
            "warning: will not write lock file of flake 'path:/x' because it has an unlocked input ('dep')\n",
        ] {
            let info = parse_metadata("f", GIT, stderr).unwrap();
            assert!(info.lock_outdated, "{stderr:?}");
            assert_eq!(info.stale, None);
            assert_eq!(info.identity(), None, "an in-memory lock isn't cacheable");
        }
        assert!(
            !parse_metadata("f", GIT, UNTRUSTED_STDERR)
                .unwrap()
                .lock_outdated
        );
    }

    #[test]
    fn inspects_colored_stderr() {
        let colored = "\x1b[35;1mwarning:\x1b[0m not writing modified lock file of flake 'x':\n\
                       • \x1b[32;1mAdded input 'dep':\x1b[0m\n    'path:/d' (2026-10-17)\n";
        assert!(parse_metadata("f", GIT, colored).unwrap().lock_outdated);
        assert_eq!(clean_stderr(colored), "");
        let error = "\x1b[31;1merror:\x1b[0m attribute '\x1b[35;1mbigmem\x1b[0m' missing";
        assert_eq!(clean_stderr(error), "error: attribute 'bigmem' missing");
        assert_eq!(strip_ansi("plain"), "plain");
        assert_eq!(strip_ansi("a\x1b[2Kb\x1b"), "ab");
    }

    fn sample_info() -> FlakeInfo {
        info(GITHUB)
    }

    #[test]
    fn identity_prefers_the_fingerprint() {
        let info = sample_info();
        assert_eq!(
            info.identity().as_deref(),
            Some("fingerprint=773b1c0a8f4cbe4ff5c09fd6c963dc4df2680450367f592cf85c58dd280bfc74")
        );
        let blank = FlakeInfo {
            fingerprint: Some(String::new()),
            ..info.clone()
        };
        assert!(blank.identity().unwrap().starts_with("narHash="));
    }

    #[test]
    fn identity_falls_back_to_the_content() {
        let base = FlakeInfo {
            fingerprint: None,
            ..sample_info()
        };
        let id = base.identity().unwrap();
        let differs = |changed: FlakeInfo| assert_ne!(changed.identity().unwrap(), id);
        differs(FlakeInfo {
            nar_hash: Some("sha256-other".into()),
            ..base.clone()
        });
        differs(FlakeInfo {
            rev: Some("1a2b3c4".into()),
            ..base.clone()
        });
        differs(FlakeInfo {
            rev: None,
            ..base.clone()
        });
        differs(FlakeInfo {
            last_modified: Some(1),
            ..base.clone()
        });
        differs(FlakeInfo {
            rev_count: Some(93),
            ..base.clone()
        });
        differs(FlakeInfo {
            locked: base.locked.replace('?', "?dir=sub&"),
            ..base.clone()
        });
        // What isn't content doesn't matter.
        let same = FlakeInfo {
            flake: "github:jjant/nix-config/main".into(),
            description: None,
            stale: Some("offline".into()),
            ..base.clone()
        };
        assert_eq!(same.identity().unwrap(), id);
    }

    #[test]
    fn identity_refuses_uncacheable_flakes() {
        let info = sample_info();
        let none = |changed: FlakeInfo| assert_eq!(changed.identity(), None);
        none(FlakeInfo {
            dirty_rev: Some("42ea902-dirty".into()),
            ..info.clone()
        });
        none(FlakeInfo {
            lock_outdated: true,
            ..info.clone()
        });
        none(FlakeInfo {
            fingerprint: None,
            nar_hash: None,
            ..info.clone()
        });
        none(FlakeInfo {
            fingerprint: None,
            nar_hash: Some(String::new()),
            ..info
        });
    }

    #[test]
    fn compare_urls() {
        let info = sample_info();
        assert_eq!(
            info.compare_url("1a2b3c4").as_deref(),
            Some(
                "https://github.com/jjant/nix-config/compare/1a2b3c4...42ea902131b93c04d098d3bcd02f29305b4bad7c"
            )
        );
        assert_eq!(
            info.compare_url("1a2b3c4-dirty").as_deref(),
            info.compare_url("1a2b3c4").as_deref()
        );
        let at = |kind: &str, locked: &str| FlakeInfo {
            kind: Some(kind.into()),
            locked: locked.into(),
            rev: Some("cafe".into()),
            ..sample_info()
        };
        assert_eq!(
            at("github", "github:o/r/cafe?host=ghe.example.com&narHash=x")
                .compare_url("beef")
                .as_deref(),
            Some("https://ghe.example.com/o/r/compare/beef...cafe")
        );
        assert_eq!(
            at("gitlab", "gitlab:veloren%2Fdev/rfcs/cafe?narHash=x")
                .compare_url("beef")
                .as_deref(),
            Some("https://gitlab.com/veloren/dev/rfcs/-/compare/beef...cafe")
        );
        assert_eq!(
            at("gitlab", "gitlab:o/r/cafe?narHash=x&host=git.example.org")
                .compare_url("beef")
                .as_deref(),
            Some("https://git.example.org/o/r/-/compare/beef...cafe")
        );
        assert_eq!(
            at("sourcehut", "sourcehut:~misterio/nix-colors/cafe?narHash=x")
                .compare_url("beef")
                .as_deref(),
            Some("https://git.sr.ht/~misterio/nix-colors/log/cafe")
        );
    }

    #[test]
    fn no_compare_url_without_a_web_forge_or_revisions() {
        let info = sample_info();
        for from in ["", "main", "1a2b3c4 ", "../../x", "-dirty", &"a".repeat(65)] {
            assert_eq!(info.compare_url(from), None, "{from:?}");
        }
        let no_rev = FlakeInfo {
            rev: None,
            ..info.clone()
        };
        assert_eq!(no_rev.compare_url("1a2b3c4"), None);
        for json in [GIT, PATH, GIT_DIRTY] {
            assert_eq!(
                parse_metadata("f", json, "").unwrap().compare_url("1a2b"),
                None
            );
        }
        let mismatched = FlakeInfo {
            kind: Some("gitlab".into()),
            ..info.clone()
        };
        assert_eq!(mismatched.compare_url("1a2b3c4"), None);
        let truncated = FlakeInfo {
            locked: "github:jjant".into(),
            ..info
        };
        assert_eq!(truncated.compare_url("1a2b3c4"), None);
    }

    #[test]
    fn decodes_percent_escapes() {
        assert_eq!(percent_decode("veloren%2Fdev"), "veloren/dev");
        assert!(matches!(percent_decode("plain"), Cow::Borrowed("plain")));
        assert_eq!(percent_decode("%C3%BC%41"), "üA");
        assert_eq!(percent_decode("100%"), "100%");
        assert_eq!(percent_decode("%zz%4"), "%zz%4");
        assert_eq!(percent_decode("%FF"), "%FF", "not UTF-8: kept");
    }

    #[test]
    fn fetchable_remotely_by_kind() {
        let at = |kind: &str, url: Option<&str>| FlakeInfo {
            kind: Some(kind.into()),
            locked_url: url.map(str::to_string),
            ..sample_info()
        };
        for (kind, url, want) in [
            ("github", None, true),
            ("gitlab", None, true),
            ("sourcehut", None, true),
            ("git", Some("https://github.com/o/r.git"), true),
            ("git", Some("ssh://git@github.com/o/r"), true),
            ("git", Some("git@github.com:o/r"), true),
            ("git", Some("file:///home/jj/nix-config"), false),
            ("git", Some("/home/jj/nix-config"), false),
            ("git", None, false),
            ("tarball", Some("https://example.com/x.tar.gz"), true),
            ("tarball", Some("file:///tmp/x.tar.gz"), false),
            ("hg", Some("https://hg.example.org/r"), true),
            ("path", None, false),
            ("indirect", None, false),
        ] {
            assert_eq!(at(kind, url).fetchable_remotely(), want, "{kind} {url:?}");
        }
        let dirty = FlakeInfo {
            dirty_rev: Some("x-dirty".into()),
            ..at("github", None)
        };
        assert!(!dirty.fetchable_remotely());
    }

    #[test]
    fn flake_info_roundtrips_through_json() {
        let info = parse_metadata("github:jjant/nix-config", GITHUB, STALE_STDERR).unwrap();
        let json = serde_json::to_string(&info).unwrap();
        assert!(json.contains("\"lockOutdated\":false"), "{json}");
        assert!(json.contains("\"narHash\":"), "{json}");
        assert_eq!(serde_json::from_str::<FlakeInfo>(&json).unwrap(), info);
    }

    #[test]
    fn parses_out_paths() {
        assert_eq!(parse_out_path(HM).unwrap().as_str(), HM);
        assert_eq!(parse_out_path(&format!("{HM}\n")).unwrap().as_str(), HM);
        assert_eq!(
            parse_out_path(&format!("{HM} \r\n\n")).unwrap().as_str(),
            HM
        );
        for (bad, want) in [
            ("", "printed nothing"),
            ("\n", "printed nothing"),
            (
                " /nix/store/ykn4vz9vpqccp1n36p50irs3dwrjfkk6-x",
                "not a store path",
            ),
            (
                "/nix/store/ykn4vz9vpqccp1n36p50irs3dwrjfkk6-x\n/nix/store/ykn4vz9vpqccp1n36p50irs3dwrjfkk6-y",
                "not a store path",
            ),
            (
                "/nix/store/ykn4vz9vpqccp1n36p50irs3dwrjfkk6-x/bin",
                "not a store path",
            ),
            ("«thunk»", "not a store path"),
        ] {
            let err = parse_out_path(bad).unwrap_err();
            assert!(err.contains(want), "{bad:?}: {err}");
        }
        let long = format!("{}\n", "x".repeat(10_000));
        assert!(parse_out_path(&long).unwrap_err().len() < 300);
    }

    #[test]
    fn parses_versions() {
        assert_eq!(
            parse_version(NIX_VERSION).as_deref(),
            Some("nix (Nix) 2.35.3")
        );
        assert_eq!(
            parse_version("nix (Determinate Nix 3.8.2) 2.30.1\n").as_deref(),
            Some("nix (Determinate Nix 3.8.2) 2.30.1")
        );
        assert_eq!(
            parse_version("  nix (Lix, like Nix) 2.93.0  \nSystem type: x86_64-linux\n").as_deref(),
            Some("nix (Lix, like Nix) 2.93.0")
        );
        assert_eq!(parse_version(""), None);
        assert_eq!(parse_version(" \n\t\n"), None);
        assert_eq!(
            parse_version("\nnix (Nix) 2.18.1").as_deref(),
            Some("nix (Nix) 2.18.1")
        );
    }

    #[test]
    fn detects_read_only_failures() {
        assert!(is_read_only_failure(IFD_STDERR));
        assert!(is_read_only_failure(TOFILE_READ_ONLY_STDERR));
        assert!(is_read_only_failure(
            "error: path '/gnu/store/wdfh0jh598v4qshmcs296ri45p0win4b-gen.drv' is not valid"
        ));
        for other in [
            "",
            MISSING_ATTR_STDERR,
            NOISE_STDERR,
            FETCH_FAILED_STDERR,
            "error: path 'relative' is not valid",
            "error: path '/nix/store/x' does not exist",
            "error: path '/nix/store/x\n' is not valid",
            "error: path '/nix/store/x",
        ] {
            assert!(!is_read_only_failure(other), "{other:?}");
        }
    }

    #[test]
    fn noise_only_stderr_cleans_to_nothing() {
        for stderr in [
            NOISE_STDERR,
            PARALLEL_NOISE_STDERR,
            GIT_DIRTY_STDERR,
            LOCK_STDERR,
            CREATING_LOCK_STDERR,
            UNTRUSTED_STDERR,
            "",
            "\n\n",
        ] {
            assert_eq!(clean_stderr(stderr), "", "{stderr:?}");
        }
    }

    #[test]
    fn clean_stderr_keeps_error_blocks() {
        assert_eq!(
            clean_stderr(MISSING_ATTR_STDERR),
            MISSING_ATTR_STDERR.trim_end()
        );
        assert_eq!(clean_stderr(IFD_STDERR), IFD_STDERR.trim_end());
        assert_eq!(
            clean_stderr(FETCH_FAILED_STDERR),
            FETCH_FAILED_STDERR.trim_end()
        );
        let noisy = format!("{NOISE_STDERR}\n{GIT_DIRTY_STDERR}{IFD_STDERR}\n\n");
        assert_eq!(clean_stderr(&noisy), IFD_STDERR.trim_end());
        assert!(
            clean_stderr(&noisy).contains("\n\n       … in the left operand"),
            "blank lines inside the trace stay"
        );
    }

    #[test]
    fn clean_stderr_drops_synthetic_noise() {
        let stderr = "\
warning: Git tree '/home/jj/nix-config' is dirty
warning: creating lock file '/home/jj/nix-config/flake.lock':
• Added input 'nixpkgs':
    'github:NixOS/nixpkgs/25f538306313eae3927264466c70d7001dcea1df?narHash=sha256-x' (2026-05-26)
• Updated input 'home-manager':
    'github:nix-community/home-manager/acd21c5' (2026-10-01)
  → 'github:nix-community/home-manager/bcd21c5' (2026-10-02)
• Removed input 'old'
warning: ignoring untrusted flake configuration setting 'extra-substituters'.
Pass '--accept-flake-config' to trust it
these 2 paths will be fetched (0.50 MiB download, 2.10 MiB unpacked):
  /nix/store/ykn4vz9vpqccp1n36p50irs3dwrjfkk6-source
  /nix/store/wqnds0yycqdlgdr8nn2xr6130l1glxaj-source
this path will be fetched (0.01 MiB download, 0.05 MiB unpacked):
  /nix/store/ykn4vz9vpqccp1n36p50irs3dwrjfkk6-source
copying path '/nix/store/ykn4vz9vpqccp1n36p50irs3dwrjfkk6-source' from 'https://cache.nixos.org'...
unpacking 'github:NixOS/nixpkgs/25f538306313eae3927264466c70d7001dcea1df' into the Git cache...
these 3 derivations will be built:
  /nix/store/wdfh0jh598v4qshmcs296ri45p0win4b-gen.drv
building '/nix/store/wdfh0jh598v4qshmcs296ri45p0win4b-gen.drv'...
error (ignored): SQLite database '/home/jj/.cache/nix/eval-cache-v6/x.sqlite' is busy
       … while doing something
evaluation warning: 'system' has been renamed to/replaced by 'stdenv.hostPlatform.system'
warning: unknown setting 'foo'
error: attribute 'bigmem' missing
       at «string»:1:1:
";
        assert_eq!(
            clean_stderr(stderr),
            "evaluation warning: 'system' has been renamed to/replaced by 'stdenv.hostPlatform.system'\n\
             warning: unknown setting 'foo'\n\
             error: attribute 'bigmem' missing\n       at «string»:1:1:"
        );
    }

    #[test]
    fn clean_stderr_keeps_stale_warnings_and_unknown_text() {
        assert_eq!(clean_stderr(STALE_STDERR), STALE_STDERR.trim_end());
        assert_eq!(
            clean_stderr("  leading indent\nplain"),
            "  leading indent\nplain"
        );
        assert_eq!(
            clean_stderr("\n\nnix: command not found\n\n"),
            "nix: command not found"
        );
    }

    #[test]
    fn summarizes_errors() {
        assert_eq!(
            error_summary(MISSING_ATTR_STDERR, "eval failed"),
            "flake 'path:/tmp/hostctl-ifd' does not provide attribute \
             'packages.x86_64-linux.nope.outPath', 'legacyPackages.x86_64-linux.nope.outPath' \
             or 'nope.outPath'"
        );
        assert_eq!(
            error_summary(IFD_STDERR, "eval failed"),
            "path '/nix/store/wdfh0jh598v4qshmcs296ri45p0win4b-gen.drv' is not valid"
        );
        assert_eq!(
            error_summary(FETCH_FAILED_STDERR, "x"),
            "unable to download 'https://api.github.com/repos/jjant/does-not-exist-hostctl/commits/HEAD': \
             Could not connect to server (7) Failed to connect to api.github.com port 443 via \
             127.0.0.1 after 0 ms: Could not connect to server"
        );
        assert_eq!(
            error_summary(&format!("{STALE_STDERR}{FETCH_FAILED_STDERR}"), "x"),
            error_summary(FETCH_FAILED_STDERR, "x"),
            "warnings mentioning errors don't count"
        );
    }

    #[test]
    fn error_summary_falls_back() {
        assert_eq!(
            error_summary(NOISE_STDERR, "nix eval failed (exit 1)"),
            "nix eval failed (exit 1)"
        );
        assert_eq!(error_summary(PARALLEL_NOISE_STDERR, "fallback"), "fallback");
        assert_eq!(error_summary("", "fallback"), "fallback");
        assert_eq!(error_summary("error:\n", "fallback"), "fallback");
        assert_eq!(error_summary("error: \n  \n", "fallback"), "fallback");
        assert_eq!(
            error_summary("sh: nix: command not found\n\n", "fallback"),
            "sh: nix: command not found"
        );
        assert_eq!(
            error_summary(
                "error: builder for '/nix/store/x.drv' failed with exit code 1;\n       last 10 log lines:\n       > boom\n",
                "fallback"
            ),
            "builder for '/nix/store/x.drv' failed with exit code 1"
        );
    }

    #[test]
    fn error_summary_ignores_source_excerpts() {
        let stderr = "\
error:
       … while evaluating the attribute 'activationPackage'
         at /nix/store/ykn4vz9vpqccp1n36p50irs3dwrjfkk6-source/flake.nix:10:5:
            9|   outputs = _: {
           10|     x = throw \"error: nope\";
             |     ^

       error: attribute 'bigmem' missing
       at /nix/store/ykn4vz9vpqccp1n36p50irs3dwrjfkk6-source/flake.nix:12:5:
           11|
           12|     y = error: \"also not this\";
             |     ^
";
        assert_eq!(error_summary(stderr, "x"), "attribute 'bigmem' missing");
    }

    #[test]
    fn attr_names_stay_identifiers_when_they_can() {
        for name in ["bigmem", "al2-x86_64", "_private", "mac'", "A1"] {
            assert!(
                matches!(attr_name(name), Cow::Borrowed(n) if n == name),
                "{name}"
            );
        }
    }

    #[test]
    fn attr_names_are_quoted_and_encoded_otherwise() {
        // Each of these was checked against nix 2.35.3 (`nix eval
        // '<flake>#.homeConfigurations.<quoted>.activationPackage'`).
        for (name, quoted) in [
            ("jj@mac", "\"jj@mac\""),
            ("u@h", "\"u@h\""),
            ("1password", "\"1password\""),
            ("-x", "\"-x\""),
            ("web1.example.com", "\"web1.example.com\""),
            ("", "\"\""),
            ("a b", "\"a%20b\""),
            ("a\tb", "\"a%09b\""),
            ("a\u{a0}b", "\"a%C2%A0b\""),
            ("pct%41", "\"pct%2541\""),
            ("a#b", "\"a%23b\""),
            ("a\"b", "\"a%22b\""),
            ("a\\b", "\"a%5Cb\""),
            ("a^b", "\"a%5Eb\""),
            ("ü", "\"%C3%BC\""),
            ("a{b}[c]|<d>`e", "\"a%7Bb%7D%5Bc%5D%7C%3Cd%3E%60e\""),
            (
                "a+b'c~d:e/f?g&h=i;j!k$l(m)n*o,p",
                "\"a+b'c~d:e/f?g&h=i;j!k$l(m)n*o,p\"",
            ),
        ] {
            assert_eq!(attr_name(name), quoted, "{name:?}");
        }
    }

    #[test]
    fn identifiers() {
        for ok in ["a", "_", "lib", "hostctl", "a-b", "a'b", "a_1", "let"] {
            assert!(is_identifier(ok), "{ok:?}");
        }
        for bad in ["", "1a", "-a", "'a", "a.b", "a b", "a$", "ä", "a\"", "${x}"] {
            assert!(!is_identifier(bad), "{bad:?}");
        }
    }

    /// Real-nix tests: only with `HOSTCTL_TEST_NIX=1` and `nix` on `PATH`;
    /// `HOSTCTL_TEST_NIX_STORE` (e.g. `local?root=$TMPDIR/store` in the
    /// sandbox) is passed as `--store`.
    mod real {
        use super::*;
        use std::collections::BTreeMap;
        use std::path::{Path, PathBuf};
        use std::process::{Command, Stdio};

        fn on_path(program: &str) -> bool {
            std::env::var_os("PATH").is_some_and(|path| {
                std::env::split_paths(&path).any(|dir| dir.join(program).is_file())
            })
        }

        fn nix_config(test: &str) -> Option<NixConfig> {
            if std::env::var("HOSTCTL_TEST_NIX").as_deref() != Ok("1") {
                eprintln!("skipping {test}: set HOSTCTL_TEST_NIX=1 to run it against nix");
                return None;
            }
            for tool in ["nix", "git"] {
                if !on_path(tool) {
                    eprintln!("skipping {test}: {tool} is not on PATH");
                    return None;
                }
            }
            let options = match std::env::var("HOSTCTL_TEST_NIX_STORE") {
                Ok(store) if !store.is_empty() => vec!["--store".to_string(), store],
                _ => Vec::new(),
            };
            Some(NixConfig {
                options,
                ..NixConfig::default()
            })
        }

        struct Run {
            ok: bool,
            stdout: String,
            stderr: String,
        }

        /// Run `spec` with Nix's per-user state inside `home`.
        fn run(spec: &CommandSpec, home: &Path) -> Run {
            let mut cmd = Command::new(&spec.program);
            cmd.args(&spec.args).stdin(Stdio::null());
            for (var, dir) in [
                ("XDG_CACHE_HOME", "cache"),
                ("XDG_CONFIG_HOME", "config"),
                ("XDG_DATA_HOME", "data"),
                ("XDG_STATE_HOME", "state"),
            ] {
                cmd.env(var, home.join(dir));
            }
            let out = cmd.output().unwrap();
            Run {
                ok: out.status.success(),
                stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
                stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
            }
        }

        fn git(dir: &Path, args: &[&str]) {
            let status = Command::new("git")
                .args(args)
                .current_dir(dir)
                .env("GIT_CONFIG_GLOBAL", "/dev/null")
                .env("GIT_CONFIG_NOSYSTEM", "1")
                .env("HOME", dir)
                .env("GIT_AUTHOR_NAME", "hostctl")
                .env("GIT_AUTHOR_EMAIL", "hostctl@localhost")
                .env("GIT_COMMITTER_NAME", "hostctl")
                .env("GIT_COMMITTER_EMAIL", "hostctl@localhost")
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status()
                .unwrap();
            assert!(status.success(), "git {args:?}");
        }

        /// A committed git flake (the spec's outputs, plus an input so that
        /// Nix would want to create `flake.lock`, plus attributes to
        /// evaluate) in `<root>/flake`.
        fn git_flake(root: &Path) -> PathBuf {
            let dep = root.join("dep");
            std::fs::create_dir(&dep).unwrap();
            std::fs::write(dep.join("file"), "dep\n").unwrap();
            let flake = root.join("flake");
            std::fs::create_dir(&flake).unwrap();
            let nix = format!(
                r#"{{
  inputs.dep = {{ url = "path:{dep}"; flake = false; }};
  outputs = _: {{
    nixosConfigurations.a = {{ }};
    darwinConfigurations = "not an attribute set";
    homeConfigurations."u@h" = {{ activationPackage.outPath = "{HM}"; }};
    homeConfigurations."ü^x".activationPackage.outPath = "{HM}2";
    lib.hostctl.hosts.a = {{ destination = "root@a.lan"; note = "x"; }};
    lib.readOnlyFails = builtins.readFile (builtins.toFile "x" "y");
  }};
}}
"#,
                dep = dep.display()
            );
            std::fs::write(flake.join("flake.nix"), nix).unwrap();
            git(&flake, &["init", "-q"]);
            git(&flake, &["add", "flake.nix"]);
            git(&flake, &["commit", "-q", "-m", "init"]);
            flake
        }

        /// Every file under `dir` except `.git` (whose index a future
        /// libgit2 might legitimately refresh).
        fn snapshot(dir: &Path) -> BTreeMap<PathBuf, Vec<u8>> {
            let mut files = BTreeMap::new();
            let mut todo = vec![dir.to_path_buf()];
            while let Some(d) = todo.pop() {
                for entry in std::fs::read_dir(&d).unwrap() {
                    let path = entry.unwrap().path();
                    if path.file_name() == Some(".git".as_ref()) {
                        continue;
                    }
                    if path.is_dir() {
                        todo.push(path);
                    } else {
                        files.insert(path.clone(), std::fs::read(&path).unwrap());
                    }
                }
            }
            files
        }

        fn resolve(nix: &NixConfig, flake: &Path, home: &Path) -> FlakeInfo {
            let url = format!("git+file://{}", flake.display());
            let out = run(&nix.metadata(&url, false), home);
            assert!(out.ok, "metadata failed: {}", out.stderr);
            parse_metadata(&url, &out.stdout, &out.stderr).unwrap()
        }

        #[test]
        fn metadata_and_hosts_leave_the_flake_untouched() {
            let Some(nix) = nix_config("metadata_and_hosts_leave_the_flake_untouched") else {
                return;
            };
            let tmp = tempfile::tempdir().unwrap();
            let flake = git_flake(tmp.path());
            let before = snapshot(&flake);

            let info = resolve(&nix, &flake, tmp.path());
            assert_eq!(info.kind.as_deref(), Some("git"));
            assert!(info.rev.is_some() && !info.is_dirty());
            assert!(info.lock_outdated, "no flake.lock: Nix locks in memory");
            assert_eq!(info.stale, None);
            assert!(info.locked.starts_with("git+file://"), "{}", info.locked);

            let out = run(&nix.hosts(&info.locked, "lib.hostctl"), tmp.path());
            assert!(out.ok, "hosts failed: {}", out.stderr);
            let hosts = parse_hosts(&out.stdout).unwrap();
            assert_eq!(
                hosts.discovered,
                vec![
                    ("a".to_string(), Kind::Nixos),
                    ("u@h".to_string(), Kind::HomeManager),
                    ("ü^x".to_string(), Kind::HomeManager),
                ]
            );
            assert_eq!(
                hosts.inventory,
                BTreeMap::from([(
                    "a".to_string(),
                    HostConfig {
                        destination: Some("root@a.lan".into()),
                        note: Some("x".into()),
                        ..HostConfig::default()
                    }
                )])
            );
            assert!(
                !clean_stderr(&out.stderr).contains("lock file"),
                "{}",
                out.stderr
            );

            assert!(!flake.join("flake.lock").exists());
            assert_eq!(snapshot(&flake), before, "the flake directory changed");
        }

        #[test]
        fn inventory_paths_are_never_evaluated_as_code() {
            let Some(nix) = nix_config("inventory_paths_are_never_evaluated_as_code") else {
                return;
            };
            let tmp = tempfile::tempdir().unwrap();
            let flake = git_flake(tmp.path());
            let info = resolve(&nix, &flake, tmp.path());
            for inventory in [
                "",
                "${builtins.abort 1}",
                "lib.${builtins.abort \"pwned\"}",
                "lib.hostctl\" or builtins.abort \"pwned",
                "let.in",
                "lib.hostctl.hosts.a.note",
                "nixosConfigurations.a",
            ] {
                let out = run(&nix.hosts(&info.locked, inventory), tmp.path());
                assert!(out.ok, "{inventory:?}: {}", out.stderr);
                let hosts = parse_hosts(&out.stdout).unwrap();
                assert!(hosts.inventory.is_empty(), "{inventory:?}");
                assert_eq!(hosts.discovered.len(), 3, "{inventory:?}");
            }
        }

        #[test]
        fn evaluates_quoted_attributes() {
            let Some(nix) = nix_config("evaluates_quoted_attributes") else {
                return;
            };
            let tmp = tempfile::tempdir().unwrap();
            let flake = git_flake(tmp.path());
            let info = resolve(&nix, &flake, tmp.path());
            for (name, want) in [("u@h", HM.to_string()), ("ü^x", format!("{HM}2"))] {
                let attr = Kind::HomeManager.default_attr(name).unwrap();
                for read_only in [true, false] {
                    let out = run(
                        &nix.eval_out_path(&info.locked, &attr, read_only),
                        tmp.path(),
                    );
                    assert!(out.ok, "{attr}: {}", out.stderr);
                    assert_eq!(parse_out_path(&out.stdout).unwrap().as_str(), want);
                }
            }

            let out = run(
                &nix.eval_out_path(&info.locked, "lib.readOnlyFails", true),
                tmp.path(),
            );
            assert!(!out.ok);
            assert!(is_read_only_failure(&out.stderr), "{}", out.stderr);
            assert!(
                error_summary(&out.stderr, "x").ends_with("is not valid"),
                "{}",
                out.stderr
            );

            let out = run(&nix.eval_out_path(&info.locked, "nope", true), tmp.path());
            assert!(!out.ok);
            assert!(!is_read_only_failure(&out.stderr), "{}", out.stderr);
            assert!(
                error_summary(&out.stderr, "x").contains("does not provide attribute"),
                "{}",
                out.stderr
            );
        }
    }

    #[test]
    fn read_only_failures_of_older_nix() {
        assert!(is_read_only_failure(
            "error: path 'lfngsssysp6h1v4ccqg23c52s9sjl779-x' is not valid"
        ));
        assert!(!is_read_only_failure("error: path 'short-x' is not valid"));
        assert_eq!(
            clean_stderr(
                "warning: you don't have Internet access; disabling some network-dependent features\nerror: boom"
            ),
            "error: boom"
        );
    }
}
