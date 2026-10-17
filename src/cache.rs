//! The on-disk cache: out paths and host lists of locked flakes (so an
//! unchanged flake costs no evaluation), the last resolution of each flake
//! (refresh TTL and stale fallback), and per-host evaluation hints. See
//! DESIGN.md ("Cache") for the format.

use std::collections::{BTreeMap, BTreeSet};
use std::ffi::{OsStr, OsString};
use std::fs::{DirBuilder, File, OpenOptions, Permissions};
use std::io::{self, Read, Write};
use std::os::fd::AsRawFd;
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::config::FlakeHosts;
use crate::nix::FlakeInfo;
use crate::storepath::StorePath;

/// File format version; a file with another version is ignored (and
/// replaced on the next save).
pub const VERSION: u32 = 2;
/// Flake entries kept (most recently used first).
pub const MAX_ENTRIES: usize = 64;

/// A bigger file isn't one hostctl wrote (64 entries stay far below this):
/// treated as corrupt rather than read into memory.
const MAX_FILE_BYTES: u64 = 32 << 20;
/// How long [`Cache::save`] waits for another process holding the lock
/// (each holds it for a few milliseconds) before giving up.
const LOCK_TIMEOUT: Duration = Duration::from_secs(2);

/// One locked flake's results.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FlakeEntry {
    /// The flake as configured.
    pub flake: String,
    pub rev: Option<String>,
    pub last_modified: Option<i64>,
    /// When this entry was last read or written (unix seconds).
    pub used: i64,
    /// attr → out path.
    #[serde(default)]
    pub outs: BTreeMap<String, StorePath>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hosts: Option<FlakeHosts>,
}

/// The last successful resolution of a flake URL.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Resolution {
    pub info: FlakeInfo,
    pub resolved_at: i64,
}

/// What hostctl learned about evaluating one attribute of one flake URL
/// (kept across revisions).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Hint {
    /// Duration of the last evaluation, to start the longest first.
    pub eval_ms: u64,
    /// `--read-only` failed (import-from-derivation): skip it next time.
    pub read_only_fails: bool,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct CacheFile {
    pub version: u32,
    /// [`Cache::key`] → entry.
    #[serde(default)]
    pub flakes: BTreeMap<String, FlakeEntry>,
    /// Flake as configured → its last good resolution.
    #[serde(default)]
    pub resolutions: BTreeMap<String, Resolution>,
    /// `<flake as configured>#<attr>` → hint.
    #[serde(default)]
    pub hints: BTreeMap<String, Hint>,
}

/// Which flake revision a store path was the desired generation of.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DeployedRev {
    pub flake: String,
    pub rev: Option<String>,
    pub last_modified: Option<i64>,
}

/// The cache, loaded in memory; [`save`](Cache::save) persists it.
#[derive(Debug, Clone, Default)]
pub struct Cache {
    /// `None` → memory only (`--no-cache`, tests, no cache dir).
    path: Option<PathBuf>,
    data: CacheFile,
    dirty: bool,
    /// Hints recorded since the last save: these beat the file's when
    /// merging; the others may be older than what other processes wrote.
    touched_hints: BTreeSet<String>,
}

impl Cache {
    /// `$XDG_CACHE_HOME/hostctl/cache.json`, else `~/.cache/hostctl/cache.json`.
    pub fn default_path() -> Option<PathBuf> {
        cache_path(std::env::var_os("XDG_CACHE_HOME"), std::env::var_os("HOME"))
    }

    /// A memory-only cache.
    pub fn in_memory() -> Cache {
        Cache::with_data(None, empty_file())
    }

    /// Load `path`; a missing, unreadable, corrupt or other-version file
    /// yields an empty cache (never an error: the cache is an optimization).
    pub fn open(path: PathBuf) -> Cache {
        let data = read_file(&path, MAX_FILE_BYTES).unwrap_or_else(empty_file);
        Cache::with_data(Some(path), data)
    }

    fn with_data(path: Option<PathBuf>, data: CacheFile) -> Cache {
        Cache {
            path,
            data,
            dirty: false,
            touched_hints: BTreeSet::new(),
        }
    }

    /// The key of `info`'s results: a hash (hex) of the full `nix --version`
    /// line, the `nix.options` argv, a non-default store directory
    /// (`NIX_STORE_DIR`) and [`FlakeInfo::identity`]. `None` (→ no caching)
    /// without an identity or a version, or when `nix_options` make
    /// evaluation impure ([`crate::nix::is_impure`]: `--impure`,
    /// `--no-pure-eval`, `--option pure-eval false`).
    pub fn key(nix_version: &str, nix_options: &[String], info: &FlakeInfo) -> Option<String> {
        let store_dir = std::env::var_os("NIX_STORE_DIR").or_else(|| std::env::var_os("NIX_STORE"));
        key_with(nix_version, nix_options, store_dir.as_deref(), info)
    }

    /// The cached out path of `attr` under `key`; marks the entry used.
    pub fn out_path(&mut self, key: &str, attr: &str, now: i64) -> Option<StorePath> {
        let entry = self.data.flakes.get_mut(key)?;
        let out = entry.outs.get(attr)?.clone();
        self.dirty |= touch(&mut entry.used, now);
        Some(out)
    }

    /// Remember that `attr` of `info` (under `key`) evaluates to `out`.
    pub fn record_out_path(
        &mut self,
        key: &str,
        info: &FlakeInfo,
        attr: &str,
        out: &StorePath,
        now: i64,
    ) {
        let entry = self.entry(key, info, now);
        if entry.outs.get(attr) != Some(out) {
            entry.outs.insert(attr.to_string(), out.clone());
        }
        self.dirty = true;
    }

    /// The cached host list under `key`; marks the entry used.
    pub fn hosts(&mut self, key: &str, now: i64) -> Option<FlakeHosts> {
        let entry = self.data.flakes.get_mut(key)?;
        let hosts = entry.hosts.clone()?;
        self.dirty |= touch(&mut entry.used, now);
        Some(hosts)
    }

    pub fn record_hosts(&mut self, key: &str, info: &FlakeInfo, hosts: &FlakeHosts, now: i64) {
        self.entry(key, info, now).hosts = Some(hosts.clone());
        self.dirty = true;
    }

    /// `key`'s entry, created for `info` if needed (evicting the least
    /// recently used one beyond [`MAX_ENTRIES`]), marked used.
    fn entry(&mut self, key: &str, info: &FlakeInfo, now: i64) -> &mut FlakeEntry {
        let flakes = &mut self.data.flakes;
        if !flakes.contains_key(key) {
            prune(flakes, MAX_ENTRIES - 1);
        }
        let entry = flakes.entry(key.to_string()).or_insert_with(|| FlakeEntry {
            flake: info.flake.clone(),
            rev: info.rev.clone(),
            last_modified: info.last_modified,
            used: now,
            outs: BTreeMap::new(),
            hosts: None,
        });
        touch(&mut entry.used, now);
        entry
    }

    /// The last good resolution of `flake` (as configured).
    pub fn resolution(&self, flake: &str) -> Option<&Resolution> {
        self.data.resolutions.get(flake)
    }

    /// Remember a successful, non-stale resolution (a stale one is ignored:
    /// it must neither restart the refresh clock nor replace a good one).
    pub fn record_resolution(&mut self, info: &FlakeInfo, now: i64) {
        if info.stale.is_some() {
            return;
        }
        let resolution = Resolution {
            info: info.clone(),
            resolved_at: now,
        };
        self.data.resolutions.insert(info.flake.clone(), resolution);
        self.dirty = true;
    }

    pub fn hint(&self, flake: &str, attr: &str) -> Option<Hint> {
        self.data.hints.get(&hint_key(flake, attr)).copied()
    }

    pub fn record_hint(&mut self, flake: &str, attr: &str, hint: Hint) {
        let key = hint_key(flake, attr);
        if self.data.hints.get(&key) != Some(&hint) {
            self.data.hints.insert(key.clone(), hint);
            self.dirty = true;
        }
        self.touched_hints.insert(key);
    }

    /// Which cached flake revision `out` was the desired generation of; the
    /// most recent (`last_modified`, then `used`) match wins.
    pub fn deployed_rev(&self, out: &StorePath) -> Option<DeployedRev> {
        let entry = self
            .data
            .flakes
            .values()
            .filter(|entry| entry.outs.values().any(|o| o == out))
            .max_by_key(|entry| (entry.last_modified, entry.used))?;
        Some(DeployedRev {
            flake: entry.flake.clone(),
            rev: entry.rev.clone(),
            last_modified: entry.last_modified,
        })
    }

    /// Persist, safely against concurrent hostctl processes: take
    /// `flock(LOCK_EX)` on a sibling `cache.lock` (waiting at most 2 s), re-read
    /// the file, merge (union of entries; same key: union of `outs`, newest
    /// `used`; newer `resolvedAt` wins; hints: ours win — those recorded since
    /// the last save), keep the [`MAX_ENTRIES`] most recently used flake
    /// entries, write a unique temp file (mode 0600) in the same directory
    /// and rename it over. The directory is hostctl's: created, or made,
    /// mode 0700. Afterwards the cache holds the merged data. No-op when
    /// nothing changed or the cache is memory-only. Blocking file IO: keep
    /// it off async worker threads.
    pub fn save(&mut self) -> io::Result<()> {
        if !self.dirty {
            return Ok(());
        }
        match self.path.clone() {
            Some(path) => self.save_to(&path, LOCK_TIMEOUT),
            None => Ok(()),
        }
    }

    fn save_to(&mut self, path: &Path, lock_timeout: Duration) -> io::Result<()> {
        let parent = path.parent().filter(|dir| !dir.as_os_str().is_empty());
        let dir = parent.unwrap_or(Path::new("."));
        DirBuilder::new().recursive(true).mode(0o700).create(dir)?;
        // hostctl's own directory: private, even if something else made it
        // (the current directory of a bare file name is left alone).
        if parent.is_some() {
            let _ = std::fs::set_permissions(dir, Permissions::from_mode(0o700));
        }
        let _lock = lock(&path.with_extension("lock"), lock_timeout)?;
        if let Some(disk) = read_file(path, MAX_FILE_BYTES) {
            merge(&mut self.data, disk, &self.touched_hints);
        }
        prune(&mut self.data.flakes, MAX_ENTRIES);
        self.data.version = VERSION;
        let mut json = serde_json::to_vec(&self.data).map_err(io::Error::other)?;
        json.push(b'\n');
        write_atomically(path, dir, &json)?;
        self.dirty = false;
        self.touched_hints.clear();
        Ok(())
    }

    pub fn data(&self) -> &CacheFile {
        &self.data
    }
}

fn empty_file() -> CacheFile {
    CacheFile {
        version: VERSION,
        ..CacheFile::default()
    }
}

/// [`Cache::default_path`] for the given `$XDG_CACHE_HOME` and `$HOME`;
/// relative or empty values are ignored, as the XDG spec asks.
fn cache_path(xdg_cache_home: Option<OsString>, home: Option<OsString>) -> Option<PathBuf> {
    let absolute = |dir: Option<OsString>| dir.map(PathBuf::from).filter(|d| d.is_absolute());
    let base = absolute(xdg_cache_home).or_else(|| Some(absolute(home)?.join(".cache")))?;
    Some(base.join("hostctl").join("cache.json"))
}

/// [`Cache::key`] with the store directory given.
fn key_with(
    nix_version: &str,
    nix_options: &[String],
    store_dir: Option<&OsStr>,
    info: &FlakeInfo,
) -> Option<String> {
    let version = nix_version.trim();
    if version.is_empty() || crate::nix::is_impure(nix_options) {
        return None;
    }
    let identity = info.identity()?;
    // Length-prefixed fields, so no two inputs serialize alike.
    let mut buf = Vec::with_capacity(128 + identity.len());
    let mut field = |bytes: &[u8]| {
        buf.extend_from_slice(bytes.len().to_string().as_bytes());
        buf.push(b':');
        buf.extend_from_slice(bytes);
    };
    field(b"hostctl-cache-key");
    field(version.as_bytes());
    field(nix_options.len().to_string().as_bytes());
    for option in nix_options {
        field(option.as_bytes());
    }
    // The default store dir counts as unset: stdenv shells export it.
    let store_dir = store_dir.filter(|d| !d.is_empty() && Path::new(d) != Path::new("/nix/store"));
    field(store_dir.map_or(&[][..], OsStr::as_encoded_bytes));
    field(identity.as_bytes());
    Some(format!("{:016x}", crate::util::fnv1a64(&buf)))
}

fn hint_key(flake: &str, attr: &str) -> String {
    format!("{flake}#{attr}")
}

/// Mark used at `now` (never backwards); whether that changed anything.
fn touch(used: &mut i64, now: i64) -> bool {
    let changed = now > *used;
    if changed {
        *used = now;
    }
    changed
}

/// Keep the `max` most recently used entries (ties: the greater key).
fn prune(flakes: &mut BTreeMap<String, FlakeEntry>, max: usize) {
    let excess = flakes.len().saturating_sub(max);
    if excess == 0 {
        return;
    }
    let mut ages: Vec<(i64, &str)> = flakes
        .iter()
        .map(|(key, entry)| (entry.used, key.as_str()))
        .collect();
    ages.sort_unstable();
    let (used, key) = ages[excess - 1];
    let newest_dropped = (used, key.to_string());
    flakes.retain(|key, entry| {
        (entry.used, key.as_str()) > (newest_dropped.0, newest_dropped.1.as_str())
    });
}

/// Fold the file's contents into ours (see [`Cache::save`] for the rules).
fn merge(ours: &mut CacheFile, theirs: CacheFile, touched_hints: &BTreeSet<String>) {
    for (key, entry) in theirs.flakes {
        match ours.flakes.get_mut(&key) {
            Some(mine) => {
                mine.used = mine.used.max(entry.used);
                for (attr, out) in entry.outs {
                    mine.outs.entry(attr).or_insert(out);
                }
                if mine.hosts.is_none() {
                    mine.hosts = entry.hosts;
                }
            }
            None => {
                ours.flakes.insert(key, entry);
            }
        }
    }
    for (flake, resolution) in theirs.resolutions {
        let newer = ours
            .resolutions
            .get(&flake)
            .is_none_or(|mine| resolution.resolved_at > mine.resolved_at);
        if newer {
            ours.resolutions.insert(flake, resolution);
        }
    }
    for (key, hint) in theirs.hints {
        if !touched_hints.contains(&key) {
            ours.hints.insert(key, hint);
        }
    }
}

/// The cache at `path`, if it is a readable regular file of at most
/// `max_bytes`, valid JSON and of this [`VERSION`].
fn read_file(path: &Path, max_bytes: u64) -> Option<CacheFile> {
    // Non-blocking, so a FIFO in its place can't hang us at open.
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NONBLOCK)
        .open(path)
        .ok()?;
    if !file.metadata().ok()?.is_file() {
        return None;
    }
    let mut bytes = Vec::new();
    file.take(max_bytes + 1).read_to_end(&mut bytes).ok()?;
    if bytes.len() as u64 > max_bytes {
        return None;
    }
    let data: CacheFile = serde_json::from_slice(&bytes).ok()?;
    (data.version == VERSION).then_some(data)
}

/// Hold `flock(LOCK_EX)` on `path` (created if needed) until the returned
/// file is dropped, waiting at most `timeout` for another holder.
fn lock(path: &Path, timeout: Duration) -> io::Result<File> {
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .open(path)?;
    let deadline = Instant::now() + timeout;
    loop {
        // SAFETY: `file` owns an open descriptor for the duration of the call.
        if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0 {
            return Ok(file);
        }
        let err = io::Error::last_os_error();
        match err.kind() {
            io::ErrorKind::Interrupted => {}
            io::ErrorKind::WouldBlock if Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(5));
            }
            io::ErrorKind::WouldBlock => {
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    format!("{} is held by another process", path.display()),
                ));
            }
            _ => return Err(err),
        }
    }
}

/// Replace `path` with `bytes` all at once: readers see the old or the new
/// file, never a partial one.
fn write_atomically(path: &Path, dir: &Path, bytes: &[u8]) -> io::Result<()> {
    let name = path.file_name().unwrap_or(OsStr::new("cache"));
    let (tmp, mut file) = create_temp(dir, name)?;
    let result = file
        .write_all(bytes)
        .and_then(|()| std::fs::rename(&tmp, path));
    if result.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    result
}

/// A new private file `<dir>/.<name>.<pid>.<counter>.<nanos>.tmp`: unique
/// across processes (pid), threads (counter) and pid reuse (time).
fn create_temp(dir: &Path, name: &OsStr) -> io::Result<(PathBuf, File)> {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos());
    let mut attempts = 0;
    loop {
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let mut file_name = OsString::from(".");
        file_name.push(name);
        file_name.push(format!(".{}.{n}.{nanos}.tmp", std::process::id()));
        let tmp = dir.join(file_name);
        match OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&tmp)
        {
            Ok(file) => return Ok((tmp, file)),
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists && attempts < 8 => attempts += 1,
            Err(e) => return Err(e),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;
    use std::sync::{Arc, Barrier};

    use crate::config::{HostConfig, Kind};

    const NIX: &str = "nix (Nix) 2.35.3";

    fn out(c: char) -> StorePath {
        StorePath::parse(&format!(
            "/nix/store/{}-home-manager-generation",
            c.to_string().repeat(32)
        ))
        .unwrap()
    }

    fn info(flake: &str, rev: &str, last_modified: i64) -> FlakeInfo {
        FlakeInfo {
            flake: flake.into(),
            locked: format!("{flake}/{rev}"),
            kind: Some("github".into()),
            locked_url: None,
            rev: Some(rev.into()),
            dirty_rev: None,
            last_modified: Some(last_modified),
            rev_count: None,
            fingerprint: Some(format!("fp-{flake}-{rev}")),
            nar_hash: Some("sha256-x".into()),
            description: None,
            stale: None,
            lock_outdated: false,
        }
    }

    fn key(info: &FlakeInfo) -> String {
        Cache::key(NIX, &[], info).unwrap()
    }

    fn hosts() -> FlakeHosts {
        FlakeHosts {
            discovered: vec![
                ("bigmem".into(), Kind::HomeManager),
                ("web1".into(), Kind::Nixos),
            ],
            inventory: BTreeMap::from([(
                "web1".to_string(),
                HostConfig {
                    destination: Some("root@web1.lan".into()),
                    note: Some("rack 3".into()),
                    ..HostConfig::default()
                },
            )]),
        }
    }

    fn temp_cache() -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("hostctl").join("cache.json");
        (dir, path)
    }

    fn files_in(dir: &Path) -> Vec<String> {
        let mut names: Vec<String> = std::fs::read_dir(dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        names
    }

    #[test]
    fn default_path_follows_xdg() {
        let path = |xdg: Option<&str>, home: Option<&str>| {
            cache_path(xdg.map(OsString::from), home.map(OsString::from))
        };
        assert_eq!(
            path(Some("/xdg/cache"), Some("/home/jj")),
            Some(PathBuf::from("/xdg/cache/hostctl/cache.json"))
        );
        for xdg in [None, Some(""), Some("relative/cache")] {
            assert_eq!(
                path(xdg, Some("/home/jj")),
                Some(PathBuf::from("/home/jj/.cache/hostctl/cache.json")),
                "{xdg:?}"
            );
        }
        assert_eq!(path(None, None), None);
        assert_eq!(path(None, Some("")), None);
        assert_eq!(path(Some("cache"), Some("home")), None);
    }

    #[test]
    fn keys_identify_nix_options_and_content() {
        let a = info("github:jjant/nix-config", "42ea902", 100);
        let k = key(&a);
        assert_eq!(k.len(), 16);
        assert!(k.bytes().all(|b| b.is_ascii_hexdigit()));
        assert_eq!(Cache::key(NIX, &[], &a.clone()), Some(k.clone()), "stable");

        let differs = |version: &str, options: &[&str], info: &FlakeInfo| {
            let options: Vec<String> = options.iter().map(|o| o.to_string()).collect();
            let other = Cache::key(version, &options, info).unwrap();
            assert_ne!(other, k, "{version:?} {options:?}");
            other
        };
        differs("nix (Nix) 2.35.2", &[], &a);
        differs("nix (Lix, like Nix) 2.35.3", &[], &a);
        differs("nix (Determinate Nix 3.8.2) 2.35.3", &[], &a);
        let ab = differs(NIX, &["a b"], &a);
        assert_ne!(differs(NIX, &["a", "b"], &a), ab);
        assert_ne!(
            differs(NIX, &["--override-input", "x", "path:/x"], &a),
            differs(NIX, &["--override-input", "x", "path:/y"], &a)
        );
        differs(NIX, &[], &info("github:jjant/nix-config", "1a2b3c4", 100));
        assert_eq!(
            Cache::key(&format!("{NIX}\n"), &[], &a),
            Some(k.clone()),
            "whitespace around the version line doesn't matter"
        );
        let renamed = FlakeInfo {
            flake: "github:jjant/nix-config/main".into(),
            ..a.clone()
        };
        assert_eq!(Cache::key(NIX, &[], &renamed), Some(k), "content, not name");
    }

    #[test]
    fn keys_include_a_non_default_store_dir() {
        let a = info("f", "1", 1);
        let plain = key_with(NIX, &[], None, &a).unwrap();
        assert_eq!(
            key_with(NIX, &[], Some("/nix/store".as_ref()), &a),
            Some(plain.clone())
        );
        assert_eq!(
            key_with(NIX, &[], Some("/nix/store/".as_ref()), &a),
            Some(plain.clone())
        );
        assert_eq!(
            key_with(NIX, &[], Some("".as_ref()), &a),
            Some(plain.clone())
        );
        assert_ne!(
            key_with(NIX, &[], Some("/gnu/store".as_ref()), &a),
            Some(plain)
        );
    }

    #[test]
    fn no_key_for_uncacheable_evaluations() {
        let a = info("f", "1", 1);
        let opts = |o: &[&str]| -> Vec<String> { o.iter().map(|s| s.to_string()).collect() };
        for impure in [
            &["--impure"][..],
            &["-L", "--impure"],
            &["--no-pure-eval"],
            &["--option", "pure-eval", "false"],
            &["--option", "pure-eval", "0"],
            &["--option", "pure-eval"],
        ] {
            assert_eq!(Cache::key(NIX, &opts(impure), &a), None, "{impure:?}");
        }
        for pure in [
            &["--option", "pure-eval", "true"][..],
            &["--option", "tarball-ttl", "0"],
            &["--impure-env"],
        ] {
            assert!(Cache::key(NIX, &opts(pure), &a).is_some(), "{pure:?}");
        }
        assert_eq!(Cache::key("", &[], &a), None, "unknown nix");
        assert_eq!(Cache::key(" \n", &[], &a), None, "unknown nix");
        let dirty = FlakeInfo {
            dirty_rev: Some("1-dirty".into()),
            ..a.clone()
        };
        assert_eq!(Cache::key(NIX, &[], &dirty), None);
        let lock = FlakeInfo {
            lock_outdated: true,
            ..a.clone()
        };
        assert_eq!(Cache::key(NIX, &[], &lock), None);
        let anonymous = FlakeInfo {
            fingerprint: None,
            nar_hash: None,
            ..a
        };
        assert_eq!(Cache::key(NIX, &[], &anonymous), None);
    }

    #[test]
    fn out_paths_roundtrip_and_mark_entries_used() {
        let mut cache = Cache::in_memory();
        let a = info("f", "1", 10);
        let k = key(&a);
        assert_eq!(cache.out_path(&k, "x", 100), None);
        assert!(!cache.dirty, "a miss changes nothing");
        cache.record_out_path(&k, &a, "x", &out('a'), 100);
        assert!(cache.dirty);
        assert_eq!(cache.out_path(&k, "x", 100), Some(out('a')));
        assert_eq!(cache.out_path(&k, "y", 100), None);
        assert_eq!(cache.out_path("other", "x", 100), None);

        cache.dirty = false;
        assert_eq!(cache.out_path(&k, "x", 100), Some(out('a')));
        assert!(!cache.dirty, "same second: nothing to persist");
        assert_eq!(cache.out_path(&k, "x", 250), Some(out('a')));
        assert!(cache.dirty);
        assert_eq!(cache.data().flakes[&k].used, 250);
        cache.out_path(&k, "x", 200);
        assert_eq!(cache.data().flakes[&k].used, 250, "never backwards");

        let entry = &cache.data().flakes[&k];
        assert_eq!(
            (
                entry.flake.as_str(),
                entry.rev.as_deref(),
                entry.last_modified
            ),
            ("f", Some("1"), Some(10))
        );
        cache.record_out_path(&k, &a, "x", &out('b'), 300);
        assert_eq!(cache.out_path(&k, "x", 300), Some(out('b')), "re-evaluated");
    }

    #[test]
    fn hosts_roundtrip() {
        let mut cache = Cache::in_memory();
        let a = info("f", "1", 10);
        let k = key(&a);
        assert_eq!(cache.hosts(&k, 1), None);
        cache.record_out_path(&k, &a, "x", &out('a'), 1);
        assert_eq!(cache.hosts(&k, 1), None, "outs alone aren't a host list");
        cache.record_hosts(&k, &a, &hosts(), 2);
        assert_eq!(cache.hosts(&k, 3), Some(hosts()));
        assert_eq!(cache.data().flakes[&k].used, 3);
        assert_eq!(cache.out_path(&k, "x", 3), Some(out('a')), "same entry");
        assert_eq!(cache.data().flakes.len(), 1);
    }

    #[test]
    fn resolutions_roundtrip() {
        let mut cache = Cache::in_memory();
        let a = info("github:o/r", "1", 10);
        assert_eq!(cache.resolution("github:o/r"), None);
        cache.record_resolution(&a, 500);
        assert_eq!(
            cache.resolution("github:o/r"),
            Some(&Resolution {
                info: a.clone(),
                resolved_at: 500
            })
        );
        let newer = info("github:o/r", "2", 20);
        cache.record_resolution(&newer, 600);
        assert_eq!(cache.resolution("github:o/r").unwrap().info, newer);
        assert_eq!(cache.resolution("github:o/other"), None);
    }

    #[test]
    fn stale_resolutions_are_not_recorded() {
        let mut cache = Cache::in_memory();
        let good = info("f", "1", 10);
        cache.record_resolution(&good, 1);
        cache.dirty = false;
        let stale = FlakeInfo {
            stale: Some("rate limited".into()),
            ..info("f", "2", 20)
        };
        cache.record_resolution(&stale, 2);
        assert_eq!(cache.resolution("f").unwrap().info, good);
        assert_eq!(cache.resolution("f").unwrap().resolved_at, 1);
        assert!(!cache.dirty);
        // A dirty tree resolved fine: its (unlocked) URL is the last good one.
        let dirty = FlakeInfo {
            rev: None,
            dirty_rev: Some("2-dirty".into()),
            ..info("f", "2", 20)
        };
        cache.record_resolution(&dirty, 3);
        assert_eq!(cache.resolution("f").unwrap().info, dirty);
    }

    #[test]
    fn hints_roundtrip() {
        let mut cache = Cache::in_memory();
        assert_eq!(cache.hint("f", "a.b"), None);
        let hint = Hint {
            eval_ms: 3400,
            read_only_fails: true,
        };
        cache.record_hint("f", "a.b", hint);
        assert!(cache.dirty);
        assert_eq!(cache.hint("f", "a.b"), Some(hint));
        assert_eq!(cache.hint("f", "a"), None);
        assert_eq!(cache.hint("g", "a.b"), None);
        assert!(cache.data().hints.contains_key("f#a.b"));
        cache.dirty = false;
        cache.record_hint("f", "a.b", hint);
        assert!(!cache.dirty, "unchanged");
    }

    #[test]
    fn deployed_rev_prefers_the_newest_revision() {
        let mut cache = Cache::in_memory();
        let old = info("f", "old", 100);
        let new = info("f", "new", 200);
        let other = info("g", "other", 300);
        cache.record_out_path(&key(&old), &old, "x", &out('a'), 50);
        cache.record_out_path(&key(&new), &new, "x", &out('a'), 10);
        cache.record_out_path(&key(&other), &other, "x", &out('b'), 90);
        assert_eq!(
            cache.deployed_rev(&out('a')),
            Some(DeployedRev {
                flake: "f".into(),
                rev: Some("new".into()),
                last_modified: Some(200),
            })
        );
        assert_eq!(
            cache.deployed_rev(&out('b')).unwrap().rev.as_deref(),
            Some("other")
        );
        assert_eq!(cache.deployed_rev(&out('c')), None);
    }

    #[test]
    fn deployed_rev_breaks_ties_by_use() {
        let mut cache = Cache::in_memory();
        let a = info("f", "a", 100);
        let b = info("g", "b", 100);
        cache.record_out_path(&key(&a), &a, "x", &out('a'), 20);
        cache.record_out_path(&key(&b), &b, "y", &out('a'), 10);
        assert_eq!(
            cache.deployed_rev(&out('a')).unwrap().rev.as_deref(),
            Some("a")
        );
        cache.out_path(&key(&b), "y", 30);
        assert_eq!(
            cache.deployed_rev(&out('a')).unwrap().rev.as_deref(),
            Some("b")
        );
        // A known commit time beats an unknown one, however recent.
        let path = FlakeInfo {
            rev: None,
            last_modified: None,
            ..info("p", "", 0)
        };
        cache.record_out_path(&key(&path), &path, "z", &out('a'), 1000);
        assert_eq!(
            cache.deployed_rev(&out('a')).unwrap().rev.as_deref(),
            Some("b")
        );
    }

    #[test]
    fn memory_keeps_the_most_recently_used_entries() {
        let mut cache = Cache::in_memory();
        for i in 0..(MAX_ENTRIES as i64 + 6) {
            let rev = format!("{i}");
            let a = info("f", &rev, i);
            cache.record_out_path(&key(&a), &a, "x", &out('a'), i);
        }
        let flakes = &cache.data().flakes;
        assert_eq!(flakes.len(), MAX_ENTRIES);
        let oldest = flakes.values().map(|e| e.used).min().unwrap();
        assert_eq!(oldest, 6);
    }

    #[test]
    fn prunes_by_use_with_deterministic_ties() {
        let entry = |used| FlakeEntry {
            used,
            ..FlakeEntry::default()
        };
        let mut flakes: BTreeMap<String, FlakeEntry> = [("a", 5), ("b", 1), ("c", 5), ("d", 3)]
            .into_iter()
            .map(|(k, used)| (k.to_string(), entry(used)))
            .collect();
        prune(&mut flakes, 4);
        assert_eq!(flakes.len(), 4);
        prune(&mut flakes, 2);
        assert_eq!(flakes.keys().collect::<Vec<_>>(), ["a", "c"]);
        prune(&mut flakes, 1);
        assert_eq!(flakes.keys().collect::<Vec<_>>(), ["c"]);
        prune(&mut flakes, 0);
        assert!(flakes.is_empty());
    }

    #[test]
    fn saves_and_reopens() {
        let (_dir, path) = temp_cache();
        let mut cache = Cache::open(path.clone());
        assert_eq!(cache.data(), &empty_file());
        let a = info("github:o/r", "1", 10);
        let k = key(&a);
        cache.record_out_path(
            &k,
            &a,
            "homeConfigurations.\"jj@mac\".activationPackage",
            &out('a'),
            100,
        );
        cache.record_hosts(&k, &a, &hosts(), 100);
        cache.record_resolution(&a, 100);
        let hint = Hint {
            eval_ms: 3400,
            read_only_fails: false,
        };
        cache.record_hint("github:o/r", "x", hint);
        cache.save().unwrap();
        assert!(!cache.dirty);

        let mut reopened = Cache::open(path.clone());
        assert_eq!(reopened.data(), cache.data());
        assert_eq!(
            reopened.out_path(&k, "homeConfigurations.\"jj@mac\".activationPackage", 101),
            Some(out('a'))
        );
        assert_eq!(reopened.hosts(&k, 101), Some(hosts()));
        assert_eq!(reopened.resolution("github:o/r").unwrap().info, a);
        assert_eq!(reopened.hint("github:o/r", "x"), Some(hint));

        let dir = path.parent().unwrap();
        assert_eq!(
            files_in(dir),
            ["cache.json", "cache.lock"],
            "no temp files left"
        );
        let mode = |p: &Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(&path), 0o600);
        assert_eq!(mode(&dir.join("cache.lock")), 0o600);
        assert_eq!(mode(dir), 0o700);
    }

    #[test]
    fn saving_makes_an_existing_directory_private() {
        let (_dir, path) = temp_cache();
        let dir = path.parent().unwrap();
        std::fs::create_dir_all(dir).unwrap();
        std::fs::set_permissions(dir, Permissions::from_mode(0o755)).unwrap();
        let mut cache = Cache::open(path.clone());
        let a = info("f", "1", 1);
        cache.record_out_path(&key(&a), &a, "x", &out('a'), 1);
        cache.save().unwrap();
        let mode = std::fs::metadata(dir).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o700);
    }

    #[test]
    fn file_format_matches_the_design() {
        let (_dir, path) = temp_cache();
        let mut cache = Cache::open(path.clone());
        let a = info("github:jjant/nix-config", "42ea902", 1792240011);
        let k = key(&a);
        cache.record_out_path(&k, &a, "attr", &out('a'), 1792240100);
        cache.record_hosts(&k, &a, &hosts(), 1792240100);
        cache.record_resolution(&a, 1792240100);
        cache.record_hint(
            "github:jjant/nix-config",
            "attr",
            Hint {
                eval_ms: 3400,
                read_only_fails: false,
            },
        );
        cache.save().unwrap();
        let json: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        let expected = serde_json::json!({
            "version": 2,
            "flakes": { (k.clone()): {
                "flake": "github:jjant/nix-config",
                "rev": "42ea902",
                "lastModified": 1792240011,
                "used": 1792240100,
                "outs": { "attr": out('a').as_str() },
                "hosts": {
                    "discovered": [["bigmem", "home-manager"], ["web1", "nixos"]],
                    "inventory": { "web1": { "destination": "root@web1.lan", "note": "rack 3" } }
                }
            } },
            "resolutions": { "github:jjant/nix-config": {
                "info": serde_json::to_value(&a).unwrap(),
                "resolvedAt": 1792240100
            } },
            "hints": { "github:jjant/nix-config#attr": { "evalMs": 3400, "readOnlyFails": false } }
        });
        assert_eq!(json, expected);
        assert_eq!(
            json["resolutions"]["github:jjant/nix-config"]["info"]["lockOutdated"],
            false
        );
    }

    #[test]
    fn save_is_a_no_op_without_changes_or_a_file() {
        let (_dir, path) = temp_cache();
        let mut cache = Cache::open(path.clone());
        cache.save().unwrap();
        assert!(!path.parent().unwrap().exists(), "nothing written");
        let mut memory = Cache::in_memory();
        let a = info("f", "1", 1);
        memory.record_out_path(&key(&a), &a, "x", &out('a'), 1);
        memory.save().unwrap();
        assert!(memory.dirty, "memory-only: nothing to save to");
        assert_eq!(memory.data().version, VERSION);
    }

    #[test]
    fn unusable_files_read_as_empty() {
        let (dir, path) = temp_cache();
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let a = info("f", "1", 1);
        let k = key(&a);
        for contents in [
            "".to_string(),
            "not json".into(),
            "{\"version\":2,\"flakes\":{\"k\":{\"flake\":1}}}".into(),
            "{\"version\":2,\"flakes\":{\"k\":{\"flake\":\"f\",\"used\":1,\"outs\":{\"x\":\"/tmp/x\"}}}}".into(),
            format!("{{\"version\":1,\"flakes\":{{\"{k}\":{{\"flake\":\"f\",\"used\":1}}}}}}"),
            format!("{{\"version\":3,\"flakes\":{{\"{k}\":{{\"flake\":\"f\",\"used\":1}}}}}}"),
            "[]".into(),
        ] {
            std::fs::write(&path, &contents).unwrap();
            let mut cache = Cache::open(path.clone());
            assert_eq!(cache.data(), &empty_file(), "{contents:?}");
            assert_eq!(cache.hosts(&k, 1), None);
        }
        // Missing files and directories too.
        assert_eq!(
            Cache::open(dir.path().join("nope.json")).data(),
            &empty_file()
        );
        assert_eq!(Cache::open(dir.path().to_path_buf()).data(), &empty_file());
    }

    #[test]
    fn other_versions_are_replaced_on_save() {
        let (_dir, path) = temp_cache();
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, r#"{"version":1,"entries":[1,2,3]}"#).unwrap();
        let mut cache = Cache::open(path.clone());
        let a = info("f", "1", 1);
        cache.record_out_path(&key(&a), &a, "x", &out('a'), 1);
        cache.save().unwrap();
        let reopened = Cache::open(path);
        assert_eq!(reopened.data().version, VERSION);
        assert_eq!(reopened.data().flakes.len(), 1);
    }

    #[test]
    fn oversized_files_are_not_read() {
        let (_dir, path) = temp_cache();
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let json = serde_json::to_vec(&empty_file()).unwrap();
        std::fs::write(&path, &json).unwrap();
        assert!(read_file(&path, json.len() as u64).is_some());
        assert!(read_file(&path, json.len() as u64 - 1).is_none());
    }

    #[test]
    fn a_fifo_in_place_of_the_file_does_not_hang() {
        let (_dir, path) = temp_cache();
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let c_path = std::ffi::CString::new(path.as_os_str().as_encoded_bytes()).unwrap();
        // SAFETY: `c_path` is a NUL-terminated path.
        assert_eq!(unsafe { libc::mkfifo(c_path.as_ptr(), 0o600) }, 0);
        assert_eq!(Cache::open(path).data(), &empty_file());
    }

    #[test]
    fn interleaved_saves_keep_both() {
        let (_dir, path) = temp_cache();
        let mut first = Cache::open(path.clone());
        let mut second = Cache::open(path.clone());
        let a = info("f", "1", 1);
        let b = info("g", "2", 2);
        first.record_out_path(&key(&a), &a, "x", &out('a'), 10);
        second.record_out_path(&key(&b), &b, "y", &out('b'), 20);
        // Same flake key from both: outs are united.
        first.record_out_path(&key(&b), &b, "z", &out('c'), 15);
        first.save().unwrap();
        second.save().unwrap();

        let mut merged = Cache::open(path.clone());
        assert_eq!(merged.out_path(&key(&a), "x", 30), Some(out('a')));
        assert_eq!(merged.out_path(&key(&b), "y", 30), Some(out('b')));
        assert_eq!(merged.out_path(&key(&b), "z", 30), Some(out('c')));
        assert_eq!(
            Cache::open(path).data().flakes[&key(&b)].used,
            20,
            "newest use"
        );
        // The saver also sees what the other process wrote.
        assert_eq!(second.out_path(&key(&a), "x", 30), Some(out('a')));
    }

    #[test]
    fn concurrent_saves_lose_nothing() {
        const WRITERS: usize = 8;
        let (_dir, path) = temp_cache();
        let barrier = Arc::new(Barrier::new(WRITERS));
        let threads: Vec<_> = (0..WRITERS)
            .map(|i| {
                let (path, barrier) = (path.clone(), Arc::clone(&barrier));
                std::thread::spawn(move || {
                    let mut cache = Cache::open(path);
                    let a = info("f", &i.to_string(), i as i64);
                    cache.record_out_path(&key(&a), &a, "x", &out('a'), i as i64);
                    cache.record_hint("f", &format!("attr{i}"), Hint::default());
                    barrier.wait();
                    cache.save().unwrap();
                })
            })
            .collect();
        for thread in threads {
            thread.join().unwrap();
        }
        let cache = Cache::open(path.clone());
        assert_eq!(cache.data().flakes.len(), WRITERS);
        assert_eq!(cache.data().hints.len(), WRITERS);
        assert_eq!(
            files_in(path.parent().unwrap()),
            ["cache.json", "cache.lock"]
        );
    }

    #[test]
    fn merge_rules() {
        let (_dir, path) = temp_cache();
        let a = info("f", "1", 1);
        let k = key(&a);
        let hint = |eval_ms| Hint {
            eval_ms,
            read_only_fails: false,
        };

        let mut disk = Cache::open(path.clone());
        disk.record_resolution(&info("old", "1", 1), 100);
        disk.record_resolution(&info("new", "1", 1), 300);
        disk.record_hint("f", "mine", hint(1));
        disk.record_hint("f", "theirs", hint(1));
        disk.record_hosts(&k, &a, &hosts(), 50);
        disk.save().unwrap();

        let mut ours = Cache::open(path.clone());
        let mut other = Cache::open(path.clone());
        other.record_hint("f", "theirs", hint(2));
        other.record_hint("f", "mine", hint(2));
        other.save().unwrap();

        ours.record_resolution(&info("old", "2", 2), 200);
        ours.record_resolution(&info("new", "2", 2), 200);
        ours.record_hint("f", "mine", hint(3));
        ours.record_out_path(&k, &a, "x", &out('a'), 60);
        ours.save().unwrap();

        let saved = Cache::open(path);
        let data = saved.data();
        assert_eq!(
            data.resolutions["old"].info.rev.as_deref(),
            Some("2"),
            "ours is newer"
        );
        assert_eq!(
            data.resolutions["new"].info.rev.as_deref(),
            Some("1"),
            "theirs is newer"
        );
        assert_eq!(data.hints["f#mine"], hint(3), "recorded here: ours wins");
        assert_eq!(data.hints["f#theirs"], hint(2), "not touched here: theirs");
        let entry = &data.flakes[&k];
        assert_eq!(entry.hosts, Some(hosts()), "kept from the file");
        assert_eq!(entry.outs.get("x"), Some(&out('a')));
        assert_eq!(entry.used, 60);
        assert_eq!(ours.data(), data, "memory holds what was written");
    }

    #[test]
    fn saves_keep_the_most_recently_used_entries() {
        let (_dir, path) = temp_cache();
        let mut first = Cache::open(path.clone());
        let mut second = Cache::open(path.clone());
        for i in 0..40 {
            let a = info("a", &i.to_string(), i);
            first.record_out_path(&key(&a), &a, "x", &out('a'), 2 * i);
            let b = info("b", &i.to_string(), i);
            second.record_out_path(&key(&b), &b, "x", &out('b'), 2 * i + 1);
        }
        first.save().unwrap();
        second.save().unwrap();
        let data = Cache::open(path).data().clone();
        assert_eq!(data.flakes.len(), MAX_ENTRIES);
        let mut used: Vec<i64> = data.flakes.values().map(|e| e.used).collect();
        used.sort_unstable();
        assert_eq!(used, (80 - MAX_ENTRIES as i64..80).collect::<Vec<_>>());
    }

    #[test]
    fn save_waits_for_the_lock_then_gives_up() {
        let (_dir, path) = temp_cache();
        let mut cache = Cache::open(path.clone());
        let a = info("f", "1", 1);
        cache.record_out_path(&key(&a), &a, "x", &out('a'), 1);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let held = lock(&path.with_extension("lock"), Duration::ZERO).unwrap();

        let err = cache.save_to(&path, Duration::from_millis(30)).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::TimedOut);
        assert!(cache.dirty, "still to be saved");
        assert!(!path.exists());

        let waiter = std::thread::spawn(move || {
            cache.save_to(&path, Duration::from_secs(5)).map(|()| cache)
        });
        std::thread::sleep(Duration::from_millis(50));
        drop(held);
        let cache = waiter.join().unwrap().unwrap();
        assert!(!cache.dirty);
    }

    #[test]
    fn temp_names_are_unique() {
        let dir = tempfile::tempdir().unwrap();
        let (first, _f) = create_temp(dir.path(), OsStr::new("cache.json")).unwrap();
        let (second, _s) = create_temp(dir.path(), OsStr::new("cache.json")).unwrap();
        assert_ne!(first, second);
        let name = first.file_name().unwrap().to_string_lossy().into_owned();
        assert!(
            name.starts_with(".cache.json.") && name.ends_with(".tmp"),
            "{name}"
        );
        assert!(
            name.contains(&format!(".{}.", std::process::id())),
            "{name}"
        );
    }

    #[test]
    fn save_reports_unwritable_locations() {
        let dir = tempfile::tempdir().unwrap();
        let blocker = dir.path().join("file");
        std::fs::write(&blocker, "").unwrap();
        let mut cache = Cache::open(blocker.join("hostctl").join("cache.json"));
        let a = info("f", "1", 1);
        cache.record_out_path(&key(&a), &a, "x", &out('a'), 1);
        assert!(cache.save().is_err());
        assert!(cache.dirty);
    }
}
