//! Closure diffs: what activating a generation would change, grouped like
//! `nix store diff-closures` (see [`crate::storepath::parse_package`]).

use std::cmp::Ordering;

use serde::Serialize;

use crate::storepath::{StorePath, compare_versions, parse_package};

/// One store path of a closure and its NAR size in bytes (0 if unknown).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClosureEntry {
    pub path: StorePath,
    pub size: u64,
}

/// How a package changed between two closures.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum ChangeKind {
    /// Not in the old closure at all.
    Added,
    /// Not in the new closure at all.
    Removed,
    /// Every removed version replaced by a newer one (Nix `compareVersions`;
    /// several versions are paired in sorted order, so `1.0` + `1.0-fish` →
    /// `1.1` + `1.1-fish` is one upgrade).
    Upgraded,
    /// Every removed version replaced by an older one.
    Downgraded,
    /// Some other change of the version set (mixed directions, different
    /// counts, or versions added/removed next to unchanged ones).
    Changed,
    /// Same versions, different size (≥ 8 KiB).
    Resized,
}

/// The change of one package name.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Change {
    pub name: String,
    /// Versions only in the old closure, sorted (byte order, like Nix).
    pub removed: Vec<String>,
    /// Versions only in the new closure, sorted.
    pub added: Vec<String>,
    /// Whether the name occurs in the old / new closure at all.
    pub in_before: bool,
    pub in_after: bool,
    /// Total size of the name's paths in the old / new closure.
    pub size_before: u64,
    pub size_after: u64,
}

impl Change {
    /// Bytes gained by the name's paths (negative when they shrink).
    pub fn size_delta(&self) -> i64 {
        signed_delta(self.size_before, self.size_after)
    }

    /// How the version set changed (see [`ChangeKind`]).
    pub fn kind(&self) -> ChangeKind {
        if !self.in_before {
            ChangeKind::Added
        } else if !self.in_after {
            ChangeKind::Removed
        } else if self.removed.is_empty() && self.added.is_empty() {
            ChangeKind::Resized
        } else if self.removed.len() == self.added.len() {
            // Pair versions in sorted order, so `1.0` + `1.0-fish` →
            // `1.1` + `1.1-fish` (home-manager's completions paths) is still
            // one upgrade.
            let mut order = self
                .removed
                .iter()
                .zip(&self.added)
                .map(|(old, new)| compare_versions(new, old));
            match order.next() {
                Some(first) if first != Ordering::Equal && order.all(|o| o == first) => {
                    if first == Ordering::Greater {
                        ChangeKind::Upgraded
                    } else {
                        ChangeKind::Downgraded
                    }
                }
                _ => ChangeKind::Changed,
            }
        } else {
            ChangeKind::Changed
        }
    }
}

/// The difference between two closures.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ClosureDiff {
    /// Sorted by name. Only names whose version set changed or whose size
    /// changed by at least [`SIZE_THRESHOLD`] bytes (Nix's rule).
    pub changes: Vec<Change>,
    pub paths_before: usize,
    pub paths_after: usize,
    pub size_before: u64,
    pub size_after: u64,
}

/// Size changes below this are noise (same threshold as Nix: 8 KiB).
pub const SIZE_THRESHOLD: u64 = 8 * 1024;

impl ClosureDiff {
    /// No package changed (the closures may still differ by small rebuilds).
    pub fn is_empty(&self) -> bool {
        self.changes.is_empty()
    }

    /// Bytes gained by the whole closure (negative when it shrinks).
    pub fn size_delta(&self) -> i64 {
        signed_delta(self.size_before, self.size_after)
    }

    /// How many changes of each kind, in [`ChangeKind`] declaration order:
    /// `[added, removed, upgraded, downgraded, changed, resized]`.
    pub fn counts(&self) -> [usize; 6] {
        let mut counts = [0; 6];
        for change in &self.changes {
            counts[change.kind() as usize] += 1;
        }
        counts
    }
}

/// `after - before`, saturated to the `i64` range: sizes come from remote
/// output, so even absurd ones must not overflow.
fn signed_delta(before: u64, after: u64) -> i64 {
    let delta = i128::from(after) - i128::from(before);
    delta.clamp(i64::MIN.into(), i64::MAX.into()) as i64
}

/// Index of the old / new closure in per-closure arrays.
const OLD: usize = 0;
const NEW: usize = 1;

/// A closure entry flattened for sorting. The field order is the sort order,
/// so each name's paths are adjacent, within them each version's, and
/// duplicates of a path in one closure end up next to each other.
#[derive(PartialEq, Eq, PartialOrd, Ord)]
struct Item<'a> {
    name: &'a str,
    version: &'a str,
    closure: usize,
    path: &'a str,
    size: u64,
}

/// Diff two closures. Duplicate paths in an input count once (with the
/// largest size given, so input order never matters). Runs in O(n log n)
/// with one allocation besides the reported changes: two 10k-path closures
/// diff in ~3 ms in release builds, mostly sorting.
pub fn diff(before: &[ClosureEntry], after: &[ClosureEntry]) -> ClosureDiff {
    let mut items = Vec::with_capacity(before.len() + after.len());
    for (closure, entries) in [(OLD, before), (NEW, after)] {
        items.extend(entries.iter().map(|entry| {
            let package = parse_package(entry.path.name());
            Item {
                name: package.name,
                version: package.version,
                closure,
                path: entry.path.as_str(),
                size: entry.size,
            }
        }));
    }
    items.sort_unstable();
    items.dedup_by(|dup, kept| {
        let same = dup.closure == kept.closure && dup.path == kept.path;
        if same {
            kept.size = kept.size.max(dup.size);
        }
        same
    });

    let mut diff = ClosureDiff::default();
    let mut paths = [0; 2];
    let mut totals = [0u64; 2];
    for group in items.chunk_by(|a, b| a.name == b.name) {
        let mut sizes = [0u64; 2];
        let mut present = [false; 2];
        // Only versions that differ allocate, and those are always reported.
        let (mut removed, mut added) = (Vec::new(), Vec::new());
        for same_version in group.chunk_by(|a, b| a.version == b.version) {
            let mut has = [false; 2];
            for item in same_version {
                has[item.closure] = true;
                sizes[item.closure] = sizes[item.closure].saturating_add(item.size);
                paths[item.closure] += 1;
            }
            let version = same_version[0].version;
            match has {
                [true, false] => removed.push(version.to_owned()),
                [false, true] => added.push(version.to_owned()),
                _ => {}
            }
            present[OLD] |= has[OLD];
            present[NEW] |= has[NEW];
        }
        for closure in [OLD, NEW] {
            totals[closure] = totals[closure].saturating_add(sizes[closure]);
        }
        if !removed.is_empty()
            || !added.is_empty()
            || sizes[OLD].abs_diff(sizes[NEW]) >= SIZE_THRESHOLD
        {
            diff.changes.push(Change {
                name: group[0].name.to_owned(),
                removed,
                added,
                in_before: present[OLD],
                in_after: present[NEW],
                size_before: sizes[OLD],
                size_after: sizes[NEW],
            });
        }
    }
    [diff.paths_before, diff.paths_after] = paths;
    [diff.size_before, diff.size_after] = totals;
    diff
}

/// Nix's rendering of a version set: `∅` when empty, otherwise the versions
/// joined with `, `, with the empty version shown as `ε` — last, as in Nix,
/// where it sorts after every (ASCII) version.
pub fn format_versions(versions: &[String]) -> String {
    if versions.is_empty() {
        return "∅".into();
    }
    let named = versions
        .iter()
        .map(String::as_str)
        .filter(|v| !v.is_empty());
    let empty = versions.iter().any(String::is_empty).then_some("ε");
    let mut out = String::new();
    for version in named.chain(empty) {
        if !out.is_empty() {
            out.push_str(", ");
        }
        out.push_str(version);
    }
    out
}

#[cfg(test)]
mod tests {
    use std::collections::{BTreeMap, BTreeSet, HashSet};
    use std::path::{Path, PathBuf};
    use std::process::Command;
    use std::time::{Duration, Instant};

    use super::*;
    use crate::util::fnv1a64;

    const BASE32: &[u8] = b"0123456789abcdfghijklmnpqrsvwxyz";

    /// splitmix64: a tiny deterministic PRNG for property tests.
    struct Rng(u64);

    impl Rng {
        fn next_u64(&mut self) -> u64 {
            self.0 = self.0.wrapping_add(0x9e37_79b9_7f4a_7c15);
            let mut z = self.0;
            z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
            z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
            z ^ (z >> 31)
        }

        fn below(&mut self, n: u64) -> u64 {
            self.next_u64() % n
        }

        fn pick<'a, T>(&mut self, items: &'a [T]) -> &'a T {
            &items[self.below(items.len() as u64) as usize]
        }

        fn shuffle<T>(&mut self, items: &mut [T]) {
            for i in (1..items.len()).rev() {
                items.swap(i, self.below(i as u64 + 1) as usize);
            }
        }
    }

    /// A store path named `name` whose hash depends on `name` and `build`:
    /// the same arguments give the same path, another `build` a rebuild.
    fn store_path(name: &str, build: u64) -> StorePath {
        let mut rng = Rng(fnv1a64(name.as_bytes()) ^ build.wrapping_mul(0x2545_f491_4f6c_dd1d));
        let hash: String = (0..32)
            .map(|_| char::from(BASE32[rng.below(32) as usize]))
            .collect();
        StorePath::parse(&format!("/nix/store/{hash}-{name}")).unwrap()
    }

    fn entry(name: &str, size: u64) -> ClosureEntry {
        ClosureEntry {
            path: store_path(name, 0),
            size,
        }
    }

    /// `name` built differently: same name, another hash.
    fn rebuilt(name: &str, size: u64) -> ClosureEntry {
        ClosureEntry {
            path: store_path(name, 1),
            size,
        }
    }

    fn strings(items: &[&str]) -> Vec<String> {
        items.iter().map(|s| s.to_string()).collect()
    }

    fn change(
        name: &str,
        removed: &[&str],
        added: &[&str],
        (in_before, in_after): (bool, bool),
        (size_before, size_after): (u64, u64),
    ) -> Change {
        Change {
            name: name.into(),
            removed: strings(removed),
            added: strings(added),
            in_before,
            in_after,
            size_before,
            size_after,
        }
    }

    fn names(diff: &ClosureDiff) -> Vec<&str> {
        diff.changes.iter().map(|c| c.name.as_str()).collect()
    }

    fn kinds(diff: &ClosureDiff) -> Vec<ChangeKind> {
        diff.changes.iter().map(Change::kind).collect()
    }

    #[test]
    fn empty_closures_diff_to_nothing() {
        let d = diff(&[], &[]);
        assert_eq!(d, ClosureDiff::default());
        assert!(d.is_empty());
        assert_eq!(d.size_delta(), 0);
        assert_eq!(d.counts(), [0; 6]);
    }

    #[test]
    fn a_new_closure_is_all_added_and_a_dropped_one_all_removed() {
        let closure = [
            entry("hello-2.12", 100),
            entry("hello-2.12-man", 50),
            entry("glibc-2.40-66", 30_000),
        ];
        let d = diff(&[], &closure);
        assert_eq!(
            d.changes,
            [
                change("glibc", &[], &["2.40-66"], (false, true), (0, 30_000)),
                change("hello", &[], &["2.12"], (false, true), (0, 150)),
            ]
        );
        assert_eq!(
            (d.paths_before, d.paths_after, d.size_before, d.size_after),
            (0, 3, 0, 30_150)
        );
        assert_eq!(d.size_delta(), 30_150);
        assert_eq!(d.counts(), [2, 0, 0, 0, 0, 0]);

        let d = diff(&closure, &[]);
        assert_eq!(
            d.changes,
            [
                change("glibc", &["2.40-66"], &[], (true, false), (30_000, 0)),
                change("hello", &["2.12"], &[], (true, false), (150, 0)),
            ]
        );
        assert_eq!(d.size_delta(), -30_150);
        assert_eq!(d.counts(), [0, 2, 0, 0, 0, 0]);
    }

    #[test]
    fn groups_paths_by_name_and_version_like_nix() {
        let before = [
            entry("atuin-18.21.0", 1000),
            // Nix strips only the last output-like suffix: version "18.21.0-fish".
            entry("atuin-18.21.0-fish-completions", 10),
            entry("brotli-1.1.0-lib", 100),
            entry("brotli-1.1.0-dev", 100),
            entry("home-manager-generation", 10),
            entry("home-manager-path", 10),
        ];
        let after = [
            entry("atuin-18.22.0", 1000),
            entry("atuin-18.22.0-fish-completions", 10),
            entry("brotli-1.2.0-lib", 100),
            entry("brotli-1.2.0-dev", 100),
            // Same name and (empty) version, another build of the same size.
            rebuilt("home-manager-generation", 10),
            entry("home-manager-path", 10),
        ];
        let d = diff(&before, &after);
        assert_eq!(
            d.changes,
            [
                change(
                    "atuin",
                    &["18.21.0", "18.21.0-fish"],
                    &["18.22.0", "18.22.0-fish"],
                    (true, true),
                    (1010, 1010)
                ),
                change("brotli", &["1.1.0"], &["1.2.0"], (true, true), (200, 200)),
            ]
        );
        assert_eq!(kinds(&d), [ChangeKind::Upgraded, ChangeKind::Upgraded]);
        assert_eq!((d.paths_before, d.paths_after), (6, 6));
    }

    #[test]
    fn version_changes_are_reported_whatever_the_size() {
        let d = diff(&[entry("foo-1.0", 0)], &[entry("foo-1.1", 0)]);
        assert_eq!(
            d.changes,
            [change("foo", &["1.0"], &["1.1"], (true, true), (0, 0))]
        );
        assert_eq!(kinds(&d), [ChangeKind::Upgraded]);
    }

    #[test]
    fn size_changes_count_from_8_kib_either_way() {
        let base: u64 = 100_000;
        for (delta, reported) in [
            (0, false),
            (8191, false),
            (8192, true),
            (-8191, false),
            (-8192, true),
            (1 << 30, true),
        ] {
            let after = base.checked_add_signed(delta).unwrap();
            let d = diff(&[entry("foo-1.0", base)], &[rebuilt("foo-1.0", after)]);
            assert_eq!(!d.is_empty(), reported, "{delta}");
            if reported {
                assert_eq!(
                    d.changes,
                    [change("foo", &[], &[], (true, true), (base, after))]
                );
                assert_eq!(d.changes[0].kind(), ChangeKind::Resized);
                assert_eq!(d.changes[0].size_delta(), delta);
            }
            assert_eq!(d.size_delta(), delta);
        }
    }

    #[test]
    fn sizes_add_up_across_a_names_versions_and_outputs() {
        // 4 KiB + 4 KiB of growth in two outputs is a reportable 8 KiB.
        let before = [entry("foo-1.0", 1000), entry("foo-1.0-dev", 1000)];
        let after = [rebuilt("foo-1.0", 5096), rebuilt("foo-1.0-dev", 5096)];
        assert_eq!(
            diff(&before, &after).changes,
            [change("foo", &[], &[], (true, true), (2000, 10_192))]
        );
    }

    #[test]
    fn versions_next_to_unchanged_ones_are_changes() {
        let shared = entry("foo-1.0", 500);
        let alone = std::slice::from_ref(&shared);
        let d = diff(&[shared.clone(), entry("foo-2.0", 500)], alone);
        assert_eq!(
            d.changes,
            [change("foo", &["2.0"], &[], (true, true), (1000, 500))]
        );
        assert_eq!(kinds(&d), [ChangeKind::Changed]);

        // `nix store diff-closures` prints this one as `foo: ∅ → ε`.
        let d = diff(alone, &[shared.clone(), entry("foo", 500)]);
        assert_eq!(
            d.changes,
            [change("foo", &[], &[""], (true, true), (500, 1000))]
        );
        assert_eq!(kinds(&d), [ChangeKind::Changed]);
    }

    #[test]
    fn duplicate_paths_count_once_with_their_largest_size() {
        let small = entry("foo-1.0", 10);
        let large = ClosureEntry {
            size: 20_000,
            ..small.clone()
        };
        for before in [
            vec![small.clone(), large.clone(), small.clone()],
            vec![large.clone(), small.clone()],
        ] {
            let d = diff(&before, &[]);
            assert_eq!((d.paths_before, d.size_before), (1, 20_000));
            assert_eq!(
                d.changes,
                [change("foo", &["1.0"], &[], (true, false), (20_000, 0))]
            );
        }

        // A path in both closures is one path in each.
        let d = diff(&[small.clone(), small.clone()], &[small]);
        assert!(d.is_empty());
        assert_eq!(
            (d.paths_before, d.paths_after, d.size_before, d.size_after),
            (1, 1, 10, 10)
        );
    }

    #[test]
    fn sorts_names_and_versions_in_byte_order() {
        let after = [
            entry("python3.13-requests-2.32.3", 1),
            entry("python3-3.13.1", 1),
            entry("zed-1.0", 1),
            entry("Zed-1.0", 1),
            entry("foo-9.0", 1),
            entry("foo-10.0", 1),
            entry("foo", 1),
            entry("foo.bar-1", 1),
            entry("foo-bar-1", 1),
        ];
        let d = diff(&[], &after);
        assert_eq!(
            names(&d),
            [
                "Zed",
                "foo",
                "foo-bar",
                "foo.bar",
                "python3",
                "python3.13-requests",
                "zed"
            ]
        );
        // Byte order, not version order.
        assert_eq!(d.changes[1].added, ["", "10.0", "9.0"]);
    }

    #[test]
    fn absurd_sizes_saturate_instead_of_overflowing() {
        let before = [entry("a-1", u64::MAX), entry("a-1-dev", u64::MAX)];
        let d = diff(&before, &[entry("b-1", u64::MAX)]);
        assert_eq!((d.size_before, d.size_after), (u64::MAX, u64::MAX));
        assert_eq!(d.size_delta(), 0);
        assert_eq!(d.changes[0].size_before, u64::MAX);
        assert_eq!(d.changes[0].size_delta(), i64::MIN);
        assert_eq!(d.changes[1].size_delta(), i64::MAX);
    }

    #[test]
    fn serializes_for_json_reports() {
        let d = diff(&[entry("foo-1.0", 10)], &[entry("foo-1.1", 20_000)]);
        assert_eq!(
            serde_json::to_value(&d).unwrap(),
            serde_json::json!({
                "changes": [{
                    "name": "foo",
                    "removed": ["1.0"],
                    "added": ["1.1"],
                    "inBefore": true,
                    "inAfter": true,
                    "sizeBefore": 10,
                    "sizeAfter": 20_000,
                }],
                "pathsBefore": 1,
                "pathsAfter": 1,
                "sizeBefore": 10,
                "sizeAfter": 20_000,
            })
        );
        assert_eq!(
            serde_json::to_value(ChangeKind::Downgraded).unwrap(),
            "downgraded"
        );
    }

    #[test]
    fn classifies_changes() {
        use ChangeKind::*;
        let kind = |removed: &[&str], added: &[&str], present| {
            change("x", removed, added, present, (0, 0)).kind()
        };
        assert_eq!(kind(&[], &["1.0"], (false, true)), Added);
        assert_eq!(kind(&[], &["", "1.0"], (false, true)), Added);
        assert_eq!(kind(&["1.0"], &[], (true, false)), Removed);
        assert_eq!(kind(&[], &[], (true, true)), Resized);
        assert_eq!(kind(&["1.0"], &["1.1"], (true, true)), Upgraded);
        // compareVersions, not byte order.
        assert_eq!(kind(&["9"], &["10"], (true, true)), Upgraded);
        assert_eq!(kind(&["10"], &["9"], (true, true)), Downgraded);
        assert_eq!(kind(&["2.3"], &["2.3pre1"], (true, true)), Downgraded);
        assert_eq!(kind(&[""], &["1.0"], (true, true)), Upgraded);
        // Different strings, equal versions.
        assert_eq!(kind(&["1.0"], &["1.00"], (true, true)), Changed);
        // Next to unchanged versions.
        assert_eq!(kind(&[], &["1.0"], (true, true)), Changed);
        assert_eq!(kind(&["1.0"], &[], (true, true)), Changed);
        // Several versions, paired in sorted order.
        assert_eq!(
            kind(&["1.0", "1.0-fish"], &["1.1", "1.1-fish"], (true, true)),
            Upgraded
        );
        assert_eq!(
            kind(&["1.1", "1.1-fish"], &["1.0", "1.0-fish"], (true, true)),
            Downgraded
        );
        assert_eq!(
            kind(&["1.0", "2.0"], &["0.9", "2.1"], (true, true)),
            Changed
        );
        assert_eq!(kind(&["1.0"], &["1.1", "1.2"], (true, true)), Changed);
    }

    #[test]
    fn counts_changes_per_kind_in_declaration_order() {
        let (mut before, mut after) = (Vec::new(), Vec::new());
        after.push(entry("added0-1", 1));
        for i in 0..2 {
            before.push(entry(&format!("removed{i}-1"), 1));
        }
        for i in 0..3 {
            before.push(entry(&format!("up{i}-1.0"), 1));
            after.push(entry(&format!("up{i}-1.1"), 1));
        }
        for i in 0..4 {
            before.push(entry(&format!("down{i}-2.0"), 1));
            after.push(entry(&format!("down{i}-1.9"), 1));
        }
        for i in 0..5 {
            before.push(entry(&format!("changed{i}-1"), 1));
            after.push(entry(&format!("changed{i}-1"), 1));
            after.push(entry(&format!("changed{i}-2"), 1));
        }
        for i in 0..6 {
            before.push(entry(&format!("resized{i}-1"), 1));
            after.push(rebuilt(&format!("resized{i}-1"), 9000));
        }
        assert_eq!(diff(&before, &after).counts(), [1, 2, 3, 4, 5, 6]);
    }

    #[test]
    fn formats_version_sets_like_nix() {
        let format = |versions: &[&str]| format_versions(&strings(versions));
        assert_eq!(format(&[]), "∅");
        assert_eq!(format(&[""]), "ε");
        assert_eq!(format(&["1.10.0"]), "1.10.0");
        assert_eq!(format(&["1.10.0", "1.10.0-fish"]), "1.10.0, 1.10.0-fish");
        // Nix puts ε into a sorted set, where it sorts after ASCII versions.
        assert_eq!(format(&["", "2.0"]), "2.0, ε");
        assert_eq!(format(&["", "1.0", "2.0"]), "1.0, 2.0, ε");
    }

    fn fixtures() -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures")
    }

    fn fixture(name: &str) -> String {
        let path = fixtures().join(name);
        std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display()))
    }

    /// `closure-hm-<generation>.tsv`: `nix-store -qR` with NAR sizes.
    fn closure(generation: u32) -> Vec<ClosureEntry> {
        fixture(&format!("closure-hm-{generation}.tsv"))
            .lines()
            .map(|line| {
                let (path, size) = line.split_once('\t').unwrap();
                ClosureEntry {
                    path: path.parse().unwrap(),
                    size: size.parse().unwrap(),
                }
            })
            .collect()
    }

    /// The `(before, after)` generations of every `diff-closures-A-B.txt`.
    fn golden_pairs() -> Vec<(u32, u32)> {
        let mut pairs: Vec<(u32, u32)> = std::fs::read_dir(fixtures())
            .unwrap()
            .filter_map(|file| {
                let name = file.unwrap().file_name().into_string().ok()?;
                let pair = name.strip_prefix("diff-closures-")?.strip_suffix(".txt")?;
                let (a, b) = pair.split_once('-')?;
                Some((a.parse().ok()?, b.parse().ok()?))
            })
            .collect();
        pairs.sort_unstable();
        assert!(pairs.len() >= 6, "{pairs:?}");
        pairs
    }

    /// One line of `nix store diff-closures`: `name: removed → added[, size]`
    /// or `name: size`.
    #[derive(Debug)]
    struct NixLine<'a> {
        name: &'a str,
        removed: Vec<String>,
        added: Vec<String>,
        size: Option<&'a str>,
    }

    fn parse_nix_line(line: &str) -> NixLine<'_> {
        let versions = |set: &str| -> Vec<String> {
            let mut versions: Vec<String> = match set {
                "∅" => Vec::new(),
                _ => set
                    .split(", ")
                    .map(|v| if v == "ε" { "" } else { v }.to_owned())
                    .collect(),
            };
            versions.sort_unstable();
            versions
        };
        let (name, rest) = line.split_once(": ").unwrap();
        let Some((removed, rest)) = rest.split_once(" → ") else {
            return NixLine {
                name,
                removed: Vec::new(),
                added: Vec::new(),
                size: Some(rest),
            };
        };
        // Versions never contain spaces; a size always does.
        let (added, size) = match rest.rsplit_once(", ") {
            Some((added, size)) if size.contains(' ') => (added, Some(size)),
            _ => (rest, None),
        };
        NixLine {
            name,
            removed: versions(removed),
            added: versions(added),
            size,
        }
    }

    /// Nix's `renderSize` (libutil/util.cc): one decimal, in KiB up to
    /// 1 MiB (even below 1 KiB), else in the largest unit it exceeds.
    fn nix_size(value: i64) -> String {
        const UNITS: [char; 9] = ['K', 'K', 'M', 'G', 'T', 'P', 'E', 'Z', 'Y'];
        let (mut unit, mut magnitude) = (0, value.unsigned_abs());
        while magnitude > 1024 && unit < UNITS.len() - 1 {
            unit += 1;
            magnitude /= 1024;
        }
        let scaled = value as f64 / 1024f64.powi(unit.max(1) as i32);
        format!("{scaled:.1} {}iB", UNITS[unit])
    }

    /// A change as `nix store diff-closures` prints it (minus colors).
    fn nix_line(change: &Change) -> String {
        let mut items = Vec::new();
        if !change.removed.is_empty() || !change.added.is_empty() {
            items.push(format!(
                "{} → {}",
                format_versions(&change.removed),
                format_versions(&change.added)
            ));
        }
        if change.size_before.abs_diff(change.size_after) >= SIZE_THRESHOLD {
            items.push(nix_size(change.size_delta()));
        }
        format!("{}: {}", change.name, items.join(", "))
    }

    #[test]
    fn agrees_with_nix_on_real_home_manager_generations() {
        for (old, new) in golden_pairs() {
            let d = diff(&closure(old), &closure(new));
            let text = fixture(&format!("diff-closures-{old}-{new}.txt"));
            let expected: Vec<NixLine> = text.lines().map(parse_nix_line).collect();
            let context = format!("generation {old} → {new}");
            assert_eq!(
                names(&d),
                expected.iter().map(|l| l.name).collect::<Vec<_>>(),
                "{context}"
            );
            for (change, line) in d.changes.iter().zip(&expected) {
                let context = format!("{context}: {}", change.name);
                assert_eq!(change.removed, line.removed, "{context}");
                assert_eq!(change.added, line.added, "{context}");
                let reported = change.size_before.abs_diff(change.size_after) >= SIZE_THRESHOLD;
                assert_eq!(reported, line.size.is_some(), "{context}");
                if let Some(size) = line.size {
                    let (number, unit) = size.split_once(' ').unwrap();
                    let power = match unit {
                        "KiB" => 1,
                        "MiB" => 2,
                        "GiB" => 3,
                        _ => panic!("{context}: unit {unit:?}"),
                    };
                    let ours = change.size_delta() as f64 / 1024f64.powi(power);
                    let theirs: f64 = number.parse().unwrap();
                    assert!(
                        (ours - theirs).abs() <= 0.05 + 1e-9,
                        "{context}: {ours} vs {size}"
                    );
                }
            }
        }
    }

    #[test]
    fn renders_real_diffs_exactly_like_nix() {
        for (old, new) in golden_pairs() {
            let d = diff(&closure(old), &closure(new));
            let lines: Vec<String> = d.changes.iter().map(nix_line).collect();
            let expected = fixture(&format!("diff-closures-{old}-{new}.txt"));
            assert_eq!(
                lines,
                expected.lines().collect::<Vec<_>>(),
                "generation {old} → {new}"
            );
        }
    }

    #[test]
    fn classifies_real_changes() {
        use ChangeKind::*;
        let cases = [
            ((1, 9), [4, 0, 0, 0, 1, 1]),
            ((5, 1), [0, 2, 0, 0, 1, 0]),
            ((5, 9), [2, 0, 0, 0, 0, 1]),
            ((8, 1), [0, 4, 0, 0, 1, 0]),
            ((8, 9), [2, 2, 0, 0, 0, 0]),
            ((9, 1), [0, 4, 0, 0, 1, 1]),
        ];
        for ((old, new), counts) in cases {
            let d = diff(&closure(old), &closure(new));
            assert_eq!(d.counts(), counts, "generation {old} → {new}");
        }
        let d = diff(&closure(1), &closure(9));
        assert_eq!(
            kinds(&d),
            [Changed, Added, Added, Resized, Added, Added],
            "{:?}",
            names(&d)
        );
    }

    #[test]
    fn totals_cover_every_distinct_path() {
        for generation in [1, 5, 8, 9] {
            let entries = closure(generation);
            let root = fixture(&format!("closure-hm-{generation}.root"));
            assert!(entries.iter().any(|e| e.path.as_str() == root.trim()));
            let distinct: HashSet<_> = entries.iter().map(|e| e.path.as_str()).collect();
            let size: u64 = entries.iter().map(|e| e.size).sum();
            let d = diff(&entries, &entries);
            assert!(d.is_empty());
            assert_eq!(
                (d.paths_before, d.paths_after),
                (distinct.len(), distinct.len())
            );
            assert_eq!((d.size_before, d.size_after), (size, size));
        }
        let d = diff(&closure(1), &closure(9));
        assert_eq!((d.paths_before, d.paths_after), (695, 702));
    }

    /// Candidate paths with clashing names, versions, outputs and builds,
    /// sized around the threshold (a path may even come with two sizes).
    fn universe(rng: &mut Rng) -> Vec<ClosureEntry> {
        const NAMES: [&str; 7] = [
            "foo",
            "foo-bar",
            "foo.bar",
            "Zed",
            "python3.13-requests",
            "hm_x.conf",
            "home-manager",
        ];
        const VERSIONS: [&str; 9] = [
            "",
            "1.0",
            "1.00",
            "1.0.1",
            "1.0pre1",
            "2.0",
            "9",
            "10",
            "2024-01-01",
        ];
        const OUTPUTS: [&str; 6] = ["", "-dev", "-bin", "-lib64", "-fish-completions", "-files"];
        let count = rng.below(60) + 1;
        (0..count)
            .map(|_| {
                let (name, version) = (rng.pick(&NAMES), rng.pick(&VERSIONS));
                let output = rng.pick(&OUTPUTS);
                let name = match *version {
                    "" => format!("{name}{output}"),
                    _ => format!("{name}-{version}{output}"),
                };
                let size = match rng.below(4) {
                    0 => 0,
                    1 => rng.below(SIZE_THRESHOLD),
                    2 => rng.below(4 * SIZE_THRESHOLD),
                    _ => rng.below(1 << 30),
                };
                ClosureEntry {
                    path: store_path(&name, rng.below(2)),
                    size,
                }
            })
            .collect()
    }

    /// A random subset of `universe`.
    fn sample(rng: &mut Rng, universe: &[ClosureEntry]) -> Vec<ClosureEntry> {
        universe
            .iter()
            .filter(|_| rng.below(2) == 0)
            .cloned()
            .collect()
    }

    /// `printClosureDiff` (nix/src/nix/diff-closures.cc), transliterated:
    /// name → version → path → size, all ordered maps.
    fn reference(before: &[ClosureEntry], after: &[ClosureEntry]) -> ClosureDiff {
        type Versions<'a> = BTreeMap<&'a str, BTreeMap<&'a str, u64>>;
        type Grouped<'a> = BTreeMap<&'a str, Versions<'a>>;
        fn group(closure: &[ClosureEntry]) -> Grouped<'_> {
            let mut grouped = Grouped::new();
            for entry in closure {
                let package = parse_package(entry.path.name());
                let size = grouped
                    .entry(package.name)
                    .or_default()
                    .entry(package.version)
                    .or_default()
                    .entry(entry.path.as_str())
                    .or_default();
                *size = (*size).max(entry.size);
            }
            grouped
        }
        fn total(versions: &Versions<'_>) -> u64 {
            versions
                .values()
                .flat_map(BTreeMap::values)
                .fold(0, |sum, size| sum.saturating_add(*size))
        }
        let (old, new) = (group(before), group(after));
        let none = Versions::new();
        let all: BTreeSet<&str> = old.keys().chain(new.keys()).copied().collect();
        let mut diff = ClosureDiff::default();
        for name in all {
            let before = old.get(name).unwrap_or(&none);
            let after = new.get(name).unwrap_or(&none);
            let only = |a: &Versions<'_>, b: &Versions<'_>| -> Vec<String> {
                a.keys()
                    .filter(|v| !b.contains_key(*v))
                    .map(|v| v.to_string())
                    .collect()
            };
            let (removed, added) = (only(before, after), only(after, before));
            let (size_before, size_after) = (total(before), total(after));
            if size_before.abs_diff(size_after) >= SIZE_THRESHOLD
                || !removed.is_empty()
                || !added.is_empty()
            {
                diff.changes.push(Change {
                    name: name.to_owned(),
                    removed,
                    added,
                    in_before: !before.is_empty(),
                    in_after: !after.is_empty(),
                    size_before,
                    size_after,
                });
            }
        }
        let paths = |g: &Grouped<'_>| {
            g.values()
                .flat_map(BTreeMap::values)
                .map(BTreeMap::len)
                .sum()
        };
        let size = |g: &Grouped<'_>| g.values().map(total).fold(0, u64::saturating_add);
        (diff.paths_before, diff.paths_after) = (paths(&old), paths(&new));
        (diff.size_before, diff.size_after) = (size(&old), size(&new));
        diff
    }

    fn mirror(diff: &ClosureDiff) -> ClosureDiff {
        ClosureDiff {
            changes: diff
                .changes
                .iter()
                .map(|c| Change {
                    name: c.name.clone(),
                    removed: c.added.clone(),
                    added: c.removed.clone(),
                    in_before: c.in_after,
                    in_after: c.in_before,
                    size_before: c.size_after,
                    size_after: c.size_before,
                })
                .collect(),
            paths_before: diff.paths_after,
            paths_after: diff.paths_before,
            size_before: diff.size_after,
            size_after: diff.size_before,
        }
    }

    fn mirror_kind(kind: ChangeKind) -> ChangeKind {
        use ChangeKind::*;
        match kind {
            Added => Removed,
            Removed => Added,
            Upgraded => Downgraded,
            Downgraded => Upgraded,
            Changed => Changed,
            Resized => Resized,
        }
    }

    #[test]
    fn matches_a_literal_port_of_nix() {
        let mut rng = Rng(1);
        for _ in 0..500 {
            let universe = universe(&mut rng);
            let (old, new) = (sample(&mut rng, &universe), sample(&mut rng, &universe));
            assert_eq!(
                diff(&old, &new),
                reference(&old, &new),
                "{old:#?}\n{new:#?}"
            );
        }
    }

    #[test]
    fn a_closure_diffed_with_itself_is_empty() {
        let mut rng = Rng(2);
        for _ in 0..300 {
            let universe = universe(&mut rng);
            let closure = sample(&mut rng, &universe);
            let d = diff(&closure, &closure);
            assert!(d.is_empty(), "{d:#?}");
            assert_eq!(
                (d.paths_before, d.size_before),
                (d.paths_after, d.size_after)
            );
        }
    }

    #[test]
    fn swapping_the_closures_mirrors_the_diff() {
        let mut rng = Rng(3);
        for _ in 0..300 {
            let universe = universe(&mut rng);
            let (old, new) = (sample(&mut rng, &universe), sample(&mut rng, &universe));
            let (forward, backward) = (diff(&old, &new), diff(&new, &old));
            assert_eq!(backward, mirror(&forward));
            assert_eq!(
                kinds(&backward),
                kinds(&forward)
                    .into_iter()
                    .map(mirror_kind)
                    .collect::<Vec<_>>()
            );
            assert_eq!(backward.size_delta(), -forward.size_delta());
        }
    }

    #[test]
    fn input_order_and_duplicates_dont_matter() {
        let scramble = |rng: &mut Rng, closure: &[ClosureEntry]| {
            let mut scrambled = closure.to_vec();
            for _ in 0..rng.below(closure.len() as u64 + 1) {
                scrambled.push(rng.pick(closure).clone());
            }
            rng.shuffle(&mut scrambled);
            scrambled
        };
        let mut rng = Rng(4);
        for _ in 0..300 {
            let universe = universe(&mut rng);
            let (old, new) = (sample(&mut rng, &universe), sample(&mut rng, &universe));
            let expected = diff(&old, &new);
            let (old, new) = (scramble(&mut rng, &old), scramble(&mut rng, &new));
            assert_eq!(diff(&old, &new), expected);
        }
    }

    /// A big system's closure (`packages` × 4 outputs) and its next
    /// generation, where every 20th package is upgraded, rebuilt 10 KiB
    /// bigger per output, downgraded, or gains a version, every 50th is
    /// dropped, and 100 packages are new.
    fn generations(packages: usize) -> (Vec<ClosureEntry>, Vec<ClosureEntry>) {
        let outputs = |p: usize, minor: u32, build: u64| {
            ["", "-dev", "-bin", "-man"].map(|output| ClosureEntry {
                path: store_path(&format!("pkg{p}-1.{minor}{output}"), build),
                size: (p as u64 * 7919) % 100_000 + 10_240 * build,
            })
        };
        let before = (0..packages).flat_map(|p| outputs(p, 5, 0)).collect();
        let mut after = Vec::new();
        for p in 0..packages + 100 {
            match p % 20 {
                _ if p % 50 == 3 && p < packages => {}
                0 => after.extend(outputs(p, 6, 0)),
                1 => after.extend(outputs(p, 5, 1)),
                2 => after.extend(outputs(p, 4, 0)),
                4 => {
                    after.extend(outputs(p, 5, 0));
                    after.push(entry(&format!("pkg{p}-2.0"), 1));
                }
                _ => after.extend(outputs(p, 5, 0)),
            }
        }
        (before, after)
    }

    #[test]
    fn diffs_twenty_thousand_paths_quickly() {
        let (before, after) = generations(5000);

        let start = Instant::now();
        let d = diff(&before, &after);
        let elapsed = start.elapsed();

        assert_eq!((d.paths_before, d.paths_after), (20_000, 20_255));
        assert_eq!(d.counts(), [100, 100, 250, 250, 250, 250]);
        assert!(elapsed < Duration::from_millis(500), "took {elapsed:?}");
    }

    /// Two closures of `builtins.toFile` paths with every kind of change, ε
    /// next to versions, several outputs, and sizes around the threshold (a
    /// file's NAR is 112 bytes plus its contents padded to 8 bytes).
    const NIX_CLOSURES: &str = r#"
      let
        f = builtins.toFile;
        x = n: builtins.concatStringsSep "" (builtins.genList (_: "x") n);
        root = paths: f "root" (toString ([ (f "keep-1.0" "k") (f "keep-1.0-dev" "kd") ] ++ paths));
      in {
        before = root [
          (f "lib-1.0" "l") (f "lib-1.0-dev" "ld") (f "tool-2.0" "t") (f "pkg-1.0" "p")
          (f "gone-0.1" "g") (f "gone-0.1-fish-completions" "gf") (f "multi-1.0" "m") (f "multi-2.0" "m")
          (f "big-1.0" (x 8)) (f "small-1.0" (x 8)) (f "shrink-1.0" (x 20000)) (f "conf-files" "c")
        ];
        after = root [
          (f "lib-1.1" "l") (f "lib-1.1-dev" "ld") (f "tool-1.9" "t") (f "pkg" "p") (f "pkg-2.0" "p")
          (f "multi-2.0" "m") (f "multi-3.0" "m") (f "new-3" "n") (f "Zed-1.0" "z")
          (f "big-1.0" (x 8200)) (f "small-1.0" (x 8192)) (f "shrink-1.0" (x 8)) (f "conf-files" (x 9000))
        ];
      }
    "#;

    fn run(command: &mut Command) -> String {
        let output = command.output().unwrap();
        assert!(
            output.status.success(),
            "{command:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8(output.stdout).unwrap()
    }

    /// Drop the color codes Nix prints even into a pipe.
    fn strip_ansi(s: &str) -> String {
        let mut out = String::with_capacity(s.len());
        let mut chars = s.chars();
        while let Some(c) = chars.next() {
            if c != '\x1b' {
                out.push(c);
            } else if chars.next() == Some('[') {
                // A CSI sequence ends at its first byte in `@`..=`~`.
                chars.by_ref().find(|c| ('@'..='~').contains(c));
            }
        }
        out
    }

    #[test]
    fn agrees_with_the_installed_nix() {
        if std::env::var("HOSTCTL_TEST_NIX").as_deref() != Ok("1") {
            eprintln!("skipped: set HOSTCTL_TEST_NIX=1 to compare with `nix store diff-closures`");
            return;
        }
        if Command::new("nix").arg("--version").output().is_err() {
            eprintln!("skipped: nix not found");
            return;
        }
        let tmp = tempfile::tempdir().unwrap();
        // A private store unless told otherwise: the test adds paths.
        let (store, _turn) = crate::util::test_nix_store();
        let store =
            store.unwrap_or_else(|| format!("local?root={}", tmp.path().join("store").display()));
        let command = |program: &str| {
            let mut command = Command::new(program);
            command
                .args(["--store", &store])
                .env("XDG_CACHE_HOME", tmp.path().join("cache"))
                .env("XDG_STATE_HOME", tmp.path().join("state"));
            if program == "nix" {
                command.args(["--extra-experimental-features", "nix-command"]);
            }
            command
        };

        let roots = run(command("nix").args(["eval", "--json", "--expr", NIX_CLOSURES]));
        let roots: BTreeMap<String, String> = serde_json::from_str(&roots).unwrap();
        let closure = |root: &str| -> Vec<ClosureEntry> {
            let paths = run(command("nix-store").args(["-qR", root]));
            let paths: Vec<&str> = paths.lines().collect();
            let sizes = run(command("nix-store").args(["-q", "--size"]).args(&paths));
            assert_eq!(sizes.lines().count(), paths.len());
            paths
                .iter()
                .zip(sizes.lines())
                .map(|(path, size)| ClosureEntry {
                    path: path.parse().unwrap(),
                    size: size.parse().unwrap(),
                })
                .collect()
        };
        let (before, after) = (&roots["before"], &roots["after"]);
        let d = diff(&closure(before), &closure(after));
        let nix = run(command("nix").args(["store", "diff-closures", before, after]));

        let ours: Vec<String> = d.changes.iter().map(nix_line).collect();
        assert_eq!(ours, strip_ansi(&nix).lines().collect::<Vec<_>>());
        use ChangeKind::*;
        assert_eq!(
            d.changes
                .iter()
                .map(|c| (c.name.as_str(), c.kind()))
                .collect::<Vec<_>>(),
            [
                ("Zed", Added),
                ("big", Resized),
                ("conf", Resized),
                ("gone", Removed),
                ("lib", Upgraded),
                ("multi", Upgraded),
                ("new", Added),
                ("pkg", Changed),
                ("shrink", Resized),
                ("tool", Downgraded),
            ]
        );
    }
}
