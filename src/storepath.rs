//! Nix store paths, and package names/versions as Nix understands them.

use std::cmp::Ordering;
use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Deserializer, Serialize, Serializer, de};

/// Nix's base32 alphabet (no `e`, `o`, `t`, `u`).
const NIX_BASE32: &[u8] = b"0123456789abcdfghijklmnpqrsvwxyz";
const HASH_LEN: usize = 32;

/// An absolute store path `<store-dir>/<32-char hash>-<name>`, e.g.
/// `/nix/store/ykn4vz9vpqccp1n36p50irs3dwrjfkk6-home-manager-generation`.
/// Paths inside a store object (`…-foo/bin/foo`) are not store paths.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct StorePath {
    path: String,
    /// Byte offset of the hash (just past the store directory's `/`).
    base: usize,
}

impl StorePath {
    pub fn parse(s: &str) -> Option<StorePath> {
        let slash = s.rfind('/')?;
        if !s.starts_with('/') || slash == 0 {
            return None;
        }
        // A clean absolute store dir: no whitespace/controls, no `.`/`..`
        // or empty components.
        let dir = &s[..slash];
        let clean_dir = dir
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"+._/-".contains(&b))
            && dir[1..]
                .split('/')
                .all(|c| !c.is_empty() && c != "." && c != "..");
        if !clean_dir {
            return None;
        }
        let base = slash + 1;
        let file = &s[base..];
        if file.len() < HASH_LEN + 2 || !file.is_char_boundary(HASH_LEN) {
            return None;
        }
        let (hash, rest) = file.split_at(HASH_LEN);
        if !hash.bytes().all(|b| NIX_BASE32.contains(&b)) {
            return None;
        }
        let name = rest.strip_prefix('-')?;
        let valid_name = !name.is_empty()
            && !name.starts_with('.')
            && name
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"+-._?=".contains(&b));
        valid_name.then(|| StorePath {
            path: s.to_string(),
            base,
        })
    }

    pub fn as_str(&self) -> &str {
        &self.path
    }

    /// `/nix/store`, usually.
    pub fn store_dir(&self) -> &str {
        &self.path[..self.base - 1]
    }

    pub fn hash(&self) -> &str {
        &self.path[self.base..self.base + HASH_LEN]
    }

    /// Everything after `<hash>-`, e.g. `home-manager-generation`.
    pub fn name(&self) -> &str {
        &self.path[self.base + HASH_LEN + 1..]
    }

    /// `ykn4vz9v-home-manager-generation`: 8 hash characters and the name.
    pub fn short(&self) -> String {
        format!("{}-{}", &self.hash()[..8], self.name())
    }
}

impl fmt::Display for StorePath {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.path)
    }
}

impl FromStr for StorePath {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        StorePath::parse(s).ok_or_else(|| format!("not a store path: {s:?}"))
    }
}

impl Serialize for StorePath {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.path)
    }
}

impl<'de> Deserialize<'de> for StorePath {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let s = String::deserialize(deserializer)?;
        StorePath::parse(&s).ok_or_else(|| de::Error::custom(format!("not a store path: {s:?}")))
    }
}

/// A store path name split the way `nix store diff-closures` groups paths.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PackageName<'a> {
    pub name: &'a str,
    pub version: &'a str,
    /// The stripped output suffix (`dev`, `bin`, `lib64`, …), or `""`.
    pub output: &'a str,
}

/// Split a store path's name like `nix store diff-closures` does: first strip
/// an output suffix (Nix's regex `(.*)-([a-z]+|lib32|lib64)`, which is
/// ambiguous with suffixes like `-unstable` in exactly the same way), then
/// split the rest like Nix's `DrvName`: the version starts after the first
/// `-` that is not followed by an ASCII letter.
///
/// `atuin-18.21.0` → (`atuin`, `18.21.0`); `brotli-1.2.0-dev` → (`brotli`,
/// `1.2.0`, output `dev`); `home-manager-generation` → (`home-manager`, ``,
/// output `generation`).
pub fn parse_package(store_name: &str) -> PackageName<'_> {
    let (base, output) = match store_name.rfind('-') {
        Some(i) if is_output_name(&store_name[i + 1..]) => (&store_name[..i], &store_name[i + 1..]),
        _ => (store_name, ""),
    };
    match base
        .as_bytes()
        .windows(2)
        .position(|w| w[0] == b'-' && !w[1].is_ascii_alphabetic())
    {
        Some(i) => PackageName {
            name: &base[..i],
            version: &base[i + 1..],
            output,
        },
        None => PackageName {
            name: base,
            version: "",
            output,
        },
    }
}

fn is_output_name(s: &str) -> bool {
    (!s.is_empty() && s.bytes().all(|b| b.is_ascii_lowercase())) || s == "lib32" || s == "lib64"
}

/// Nix's `builtins.compareVersions`: versions are split into components at
/// `.`/`-` and digit/non-digit boundaries; numeric components compare as
/// numbers, a missing component sorts before a number, `pre` sorts before
/// anything else, and a non-number sorts before a number (`2.3a` < `2.3.1`).
pub fn compare_versions(a: &str, b: &str) -> Ordering {
    let (mut pa, mut pb) = (a, b);
    while !pa.is_empty() || !pb.is_empty() {
        let (ca, ra) = next_component(pa);
        let (cb, rb) = next_component(pb);
        pa = ra;
        pb = rb;
        if component_lt(ca, cb) {
            return Ordering::Less;
        }
        if component_lt(cb, ca) {
            return Ordering::Greater;
        }
    }
    Ordering::Equal
}

/// The next version component of `s` and the remainder.
fn next_component(s: &str) -> (&str, &str) {
    let s = s.trim_start_matches(['.', '-']);
    let end = match s.bytes().next() {
        None => 0,
        Some(b) if b.is_ascii_digit() => s
            .bytes()
            .position(|b| !b.is_ascii_digit())
            .unwrap_or(s.len()),
        Some(_) => s
            .bytes()
            .position(|b| b.is_ascii_digit() || b == b'.' || b == b'-')
            .unwrap_or(s.len()),
    };
    s.split_at(end)
}

fn component_lt(c1: &str, c2: &str) -> bool {
    let n1 = as_number(c1);
    let n2 = as_number(c2);
    match (n1, n2) {
        (Some(x), Some(y)) => x < y,
        _ if c1.is_empty() && n2.is_some() => true,
        _ if c1 == "pre" && c2 != "pre" => true,
        _ if c2 == "pre" => false,
        (_, Some(_)) => true,
        (Some(_), _) => false,
        _ => c1 < c2,
    }
}

/// Nix's `string2Int<int>`: a component is a number only if it fits an
/// `int` (so `2147483648` and timestamp-like `20240101123456` are strings).
fn as_number(c: &str) -> Option<i32> {
    if c.is_empty() || !c.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    c.parse().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    const HM: &str = "/nix/store/ykn4vz9vpqccp1n36p50irs3dwrjfkk6-home-manager-generation";

    #[test]
    fn parses_store_paths() {
        let p = StorePath::parse(HM).unwrap();
        assert_eq!(p.as_str(), HM);
        assert_eq!(p.store_dir(), "/nix/store");
        assert_eq!(p.hash(), "ykn4vz9vpqccp1n36p50irs3dwrjfkk6");
        assert_eq!(p.name(), "home-manager-generation");
        assert_eq!(p.short(), "ykn4vz9v-home-manager-generation");
        assert_eq!(p.to_string(), HM);
        let custom = StorePath::parse("/gnu/store/ykn4vz9vpqccp1n36p50irs3dwrjfkk6-x").unwrap();
        assert_eq!(custom.store_dir(), "/gnu/store");
    }

    #[test]
    fn rejects_non_store_paths() {
        for bad in [
            "",
            "nix/store/ykn4vz9vpqccp1n36p50irs3dwrjfkk6-x",
            "/ykn4vz9vpqccp1n36p50irs3dwrjfkk6-x",
            "/nix/store/ykn4vz9vpqccp1n36p50irs3dwrjfkk6",
            "/nix/store/ykn4vz9vpqccp1n36p50irs3dwrjfkk6-",
            "/nix/store/ykn4vz9vpqccp1n36p50irs3dwrjfkk6_x",
            "/nix/store/eeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee-x",
            "/nix/store/ykn4vz9vpqccp1n36p50irs3dwrjfk-x",
            "/nix/store/ykn4vz9vpqccp1n36p50irs3dwrjfkk6-x/bin/x",
            "/nix/store/ykn4vz9vpqccp1n36p50irs3dwrjfkk6-.hidden",
            "/nix/store/ykn4vz9vpqccp1n36p50irs3dwrjfkk6-has space",
            "/nix/store/ykn4vz9vpqccp1n36p50irs3dwrjfkk6-x\n",
            "/nix/store/ykn4vz9vpqccp1n36p50irs3dwrjfkkü-x",
            "/nix/store/ykn4vz9vpqccp1n36p50irs3dwrjfkk6-x\n/nix/store/ykn4vz9vpqccp1n36p50irs3dwrjfkk6-y",
            "/x y/nix/store/ykn4vz9vpqccp1n36p50irs3dwrjfkk6-z",
            "/nix//store/ykn4vz9vpqccp1n36p50irs3dwrjfkk6-z",
            "/nix/../store/ykn4vz9vpqccp1n36p50irs3dwrjfkk6-z",
        ] {
            assert_eq!(StorePath::parse(bad), None, "{bad:?}");
        }
    }

    #[test]
    fn serde_roundtrip() {
        let p = StorePath::parse(HM).unwrap();
        let json = serde_json::to_string(&p).unwrap();
        assert_eq!(json, format!("\"{HM}\""));
        assert_eq!(serde_json::from_str::<StorePath>(&json).unwrap(), p);
        assert!(serde_json::from_str::<StorePath>("\"/tmp/x\"").is_err());
    }

    fn split(name: &str) -> (&str, &str, &str) {
        let p = parse_package(name);
        (p.name, p.version, p.output)
    }

    #[test]
    fn splits_package_names_like_nix() {
        assert_eq!(split("atuin-18.21.0"), ("atuin", "18.21.0", ""));
        assert_eq!(split("brotli-1.2.0-dev"), ("brotli", "1.2.0", "dev"));
        assert_eq!(split("brotli-1.2.0-lib"), ("brotli", "1.2.0", "lib"));
        assert_eq!(split("glibc-2.44-25"), ("glibc", "2.44-25", ""));
        assert_eq!(
            split("home-manager-generation"),
            ("home-manager", "", "generation")
        );
        assert_eq!(
            split("hm_tmuxwindownaming"),
            ("hm_tmuxwindownaming", "", "")
        );
        assert_eq!(
            split("atuin-server.service"),
            ("atuin-server.service", "", "")
        );
        // Nix's own ambiguity: "-completions" is taken as an output name.
        assert_eq!(
            split("atuin-18.21.0-fish-completions"),
            ("atuin", "18.21.0-fish", "completions")
        );
        assert_eq!(split("gcc-14.2.0-lib64"), ("gcc", "14.2.0", "lib64"));
        assert_eq!(
            split("python3.13-requests-2.32.3"),
            ("python3.13-requests", "2.32.3", "")
        );
        assert_eq!(split("nss-cacert-3.115"), ("nss-cacert", "3.115", ""));
        assert_eq!(
            split("foo-unstable-2024-01-01"),
            ("foo-unstable", "2024-01-01", "")
        );
        assert_eq!(split("x"), ("x", "", ""));
    }

    #[test]
    fn compares_versions_like_nix() {
        use Ordering::*;
        // Cases from Nix's own tests (tests/functional/lang/eval-okay-versions.nix).
        let cases = [
            ("1.0", "2.3", Less),
            ("2.1", "2.3", Less),
            ("2.3", "2.3", Equal),
            ("2.5", "2.3", Greater),
            ("3.1", "2.3", Greater),
            ("2.3.1", "2.3", Greater),
            ("2.3.1", "2.3a", Greater),
            ("2.3pre1", "2.3", Less),
            ("2.3pre3", "2.3pre12", Less),
            ("2.3a", "2.3c", Less),
            ("2.3pre1", "2.3c", Less),
            ("2.3pre1", "2.3q", Less),
            ("1.0", "1.0.1", Less),
            ("10", "9", Greater),
            ("1.0-rc1", "1.0", Greater),
            ("", "1", Less),
            ("2024-01-01", "2023-12-31", Greater),
            // Components beyond i32 are not numbers for Nix.
            ("2147483647", "9", Greater),
            ("2147483648", "9", Less),
            ("20240101123456", "20240101", Less),
        ];
        for (a, b, want) in cases {
            assert_eq!(compare_versions(a, b), want, "{a} vs {b}");
            assert_eq!(compare_versions(b, a), want.reverse(), "{b} vs {a}");
        }
    }
}
