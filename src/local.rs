//! Which configured hosts are this machine (probed and activated without
//! ssh). See DESIGN.md ("Self detection").

use std::ffi::CStr;
use std::net::IpAddr;

use crate::config::Kind;
use crate::remote::Destination;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Os {
    Linux,
    Darwin,
    Other,
}

impl Os {
    pub fn current() -> Os {
        if cfg!(target_os = "macos") {
            Os::Darwin
        } else if cfg!(target_os = "linux") {
            Os::Linux
        } else {
            Os::Other
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LocalInfo {
    /// This machine's hostname as the OS reports it (with or without domain).
    pub hostname: String,
    /// The current user (`$USER`, else `getpwuid`, else empty).
    pub user: String,
    pub os: Os,
    /// A host forced to be the local one (`--self`, `HOSTCTL_SELF`, `self`).
    pub forced: Option<String>,
}

impl LocalInfo {
    /// The running machine: hostname from `gethostname(2)` (falling back to
    /// `$HOSTNAME`, then `localhost`), user, [`Os::current`]. An empty
    /// `forced` name counts as none. Runs no processes.
    pub fn detect(forced: Option<String>) -> LocalInfo {
        LocalInfo {
            hostname: pick_hostname(os_hostname(), || env_value("HOSTNAME")),
            user: pick_user(env_value("USER"), passwd_user),
            os: Os::current(),
            forced: forced.filter(|name| !name.is_empty()),
        }
    }
}

fn pick_hostname(os: Option<String>, env: impl FnOnce() -> Option<String>) -> String {
    os.or_else(env).unwrap_or_else(|| "localhost".to_owned())
}

/// The passwd lookup only runs without `$USER` (it may ask a directory
/// service over the network).
fn pick_user(env: Option<String>, passwd: impl FnOnce() -> Option<String>) -> String {
    env.or_else(passwd).unwrap_or_default()
}

/// A non-empty environment variable, lossily decoded.
fn env_value(key: &str) -> Option<String> {
    let value = std::env::var_os(key)?;
    (!value.is_empty()).then(|| value.to_string_lossy().into_owned())
}

/// `gethostname(2)` as lossy UTF-8; `None` on failure or an empty name.
fn os_hostname() -> Option<String> {
    // Linux allows 64 bytes, macOS 255.
    let mut buf = [0u8; 256];
    // SAFETY: `buf` is valid for writes of `buf.len()` bytes.
    let rc = unsafe { libc::gethostname(buf.as_mut_ptr().cast(), buf.len()) };
    if rc != 0 {
        return None;
    }
    c_name(&buf)
}

/// The NUL-terminated name at the start of `buf` (all of it without a NUL:
/// POSIX leaves it out when a name had to be truncated), lossy UTF-8;
/// `None` when empty.
fn c_name(buf: &[u8]) -> Option<String> {
    let len = buf.iter().position(|&b| b == 0).unwrap_or(buf.len());
    let name = String::from_utf8_lossy(&buf[..len]);
    (!name.is_empty()).then(|| name.into_owned())
}

/// The effective user's name in the passwd database
/// (`getpwuid_r(geteuid())`); `None` without an entry (e.g. a container
/// running as an arbitrary uid).
fn passwd_user() -> Option<String> {
    const ATTEMPTS: usize = 8;
    // SAFETY: `sysconf` and `geteuid` have no preconditions.
    let (hint, uid) = unsafe { (libc::sysconf(libc::_SC_GETPW_R_SIZE_MAX), libc::geteuid()) };
    let mut buf = vec![0u8; usize::try_from(hint).unwrap_or(0).clamp(256, 16 << 10)];
    for _ in 0..ATTEMPTS {
        // SAFETY: `passwd` is plain old data (integers and pointers), for
        // which all zeroes is a valid value.
        let mut entry: libc::passwd = unsafe { std::mem::zeroed() };
        let mut found: *mut libc::passwd = std::ptr::null_mut();
        // SAFETY: every pointer is valid for writes (`buf` for `buf.len()`
        // bytes); the strings in `entry` point into `buf`.
        let rc = unsafe {
            libc::getpwuid_r(
                uid,
                &mut entry,
                buf.as_mut_ptr().cast(),
                buf.len(),
                &mut found,
            )
        };
        match rc {
            libc::ERANGE => buf.resize(buf.len() * 2, 0),
            libc::EINTR => {}
            0 if !found.is_null() && !entry.pw_name.is_null() => {
                // SAFETY: on success `pw_name` is a NUL-terminated string
                // inside `buf`, which outlives this borrow.
                let name = unsafe { CStr::from_ptr(entry.pw_name) };
                let name = name.to_string_lossy();
                return (!name.is_empty()).then(|| name.into_owned());
            }
            _ => return None,
        }
    }
    None
}

/// What [`local_hosts`] needs to know about a host.
#[derive(Debug, Clone, Copy)]
pub struct Candidate<'a> {
    pub name: &'a str,
    pub kind: Kind,
    pub hostnames: &'a [String],
    /// The parsed destination, if one was configured (`None` = defaulted to
    /// the name).
    pub destination: Option<&'a Destination>,
}

/// How a candidate relates to this machine.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Locality {
    Remote,
    Local,
    /// Local only by the "single darwin host on a Mac" fallback.
    Assumed,
}

/// The [`Locality`] of each candidate (same order). Precedence:
///
/// 1. `local.forced`: exactly that candidate is `Local`; a name no candidate
///    has is an error.
/// 2. Vetoes for rule 3: a destination whose user differs from
///    `local.user`, with a port other than 22, or a loopback host
///    (`localhost`, `127.*`, `::1`; those are tunnels to other machines).
/// 3. Otherwise `Local` when the destination host or one of `hostnames` is
///    [`same_host`] as `local.hostname`. The bare name counts only when the
///    candidate has neither a dotted destination host nor dotted hostnames
///    (an explicit FQDN that doesn't match vetoes it; so does an IPv6
///    address).
/// 4. If nothing is local and `local.os` is Darwin: the only `darwin`
///    candidate whose destination is unset or loopback becomes `Assumed`.
///
/// A bare name of the form `user@host` (home-manager's convention) is read
/// like the destination it defaults to: `user` must be `local.user`.
pub fn local_hosts(
    candidates: &[Candidate<'_>],
    local: &LocalInfo,
) -> Result<Vec<Locality>, String> {
    if let Some(forced) = local.forced.as_deref() {
        let Some(at) = candidates.iter().position(|c| c.name == forced) else {
            return Err(unknown_self(forced, candidates));
        };
        let mut localities = vec![Locality::Remote; candidates.len()];
        localities[at] = Locality::Local;
        return Ok(localities);
    }
    let mut localities: Vec<Locality> = candidates
        .iter()
        .map(|c| {
            if is_this_machine(c, local) {
                Locality::Local
            } else {
                Locality::Remote
            }
        })
        .collect();
    if local.os == Os::Darwin && !localities.contains(&Locality::Local) {
        let mut macs = candidates.iter().enumerate().filter(|(_, c)| {
            c.kind == Kind::Darwin && c.destination.is_none_or(|d| is_loopback(&d.host))
        });
        if let (Some((at, _)), None) = (macs.next(), macs.next()) {
            localities[at] = Locality::Assumed;
        }
    }
    Ok(localities)
}

/// Rules 2 and 3 of [`local_hosts`].
fn is_this_machine(c: &Candidate<'_>, local: &LocalInfo) -> bool {
    let here = |name: &str| same_host(name, &local.hostname);
    if let Some(d) = c.destination {
        let vetoed = d.user.as_deref().is_some_and(|user| user != local.user)
            || d.port.is_some_and(|port| port != 22)
            || is_loopback(&d.host);
        if vetoed {
            return false;
        }
        if here(&d.host) {
            return true;
        }
    }
    if c.hostnames.iter().any(|h| here(h)) {
        return true;
    }
    let qualified = c.destination.is_some_and(|d| is_qualified(&d.host))
        || c.hostnames.iter().any(|h| is_qualified(h));
    if qualified {
        return false;
    }
    match c.name.rsplit_once('@') {
        Some((user, host)) => user == local.user && here(host),
        None => here(c.name),
    }
}

/// A dotted name or an IP address: explicit enough that not matching means
/// another machine.
fn is_qualified(host: &str) -> bool {
    host.contains(['.', ':'])
}

fn unknown_self(forced: &str, candidates: &[Candidate<'_>]) -> String {
    const SHOWN: usize = 10;
    let mut names: Vec<&str> = candidates.iter().map(|c| c.name).collect();
    names.sort_unstable();
    // Names may come from a flake: no control characters to the terminal.
    let shown = names
        .iter()
        .take(SHOWN)
        .map(|n| crate::util::sanitize_terminal(n));
    let mut known = shown.collect::<Vec<_>>().join(", ");
    if names.len() > SHOWN {
        known.push_str(", …");
    }
    if known.is_empty() {
        known.push_str("none");
    }
    format!("unknown host {forced:?} in --self, HOSTCTL_SELF or self (hosts: {known})")
}

/// Whether two host names denote the same machine: equal ignoring ASCII case;
/// or, when exactly one has no dot, it equals the other's first label
/// (`bigmem` ~ `bigmem.example.com`). Two dotted names must match fully.
/// Empty names never match.
///
/// A trailing root dot is ignored (`bigmem.example.com.`), and IP addresses
/// only match exactly (`10` isn't `10.0.0.5`).
pub fn same_host(a: &str, b: &str) -> bool {
    let (a, b) = (without_root(a), without_root(b));
    if a.is_empty() || b.is_empty() {
        return false;
    }
    if a.eq_ignore_ascii_case(b) {
        return true;
    }
    if a.parse::<IpAddr>().is_ok() || b.parse::<IpAddr>().is_ok() {
        return false;
    }
    match (a.split_once('.'), b.split_once('.')) {
        (None, Some((first, _))) => a.eq_ignore_ascii_case(first),
        (Some((first, _)), None) => b.eq_ignore_ascii_case(first),
        _ => false,
    }
}

fn without_root(host: &str) -> &str {
    host.strip_suffix('.').unwrap_or(host)
}

/// `localhost`, `localhost.localdomain`, `127.0.0.0/8`, `::1` (any case,
/// trailing dot, brackets or IPv4-mapped form; `127.1` too, which
/// `getaddrinfo` reads as `127.0.0.1`).
pub fn is_loopback(host: &str) -> bool {
    let host = host
        .strip_prefix('[')
        .and_then(|h| h.strip_suffix(']'))
        .unwrap_or(host);
    let host = without_root(host);
    if host.eq_ignore_ascii_case("localhost") || host.eq_ignore_ascii_case("localhost.localdomain")
    {
        return true;
    }
    if let Some(rest) = host.strip_prefix("127.") {
        return !rest.is_empty() && rest.bytes().all(|b| b.is_ascii_digit() || b == b'.');
    }
    match host.parse::<IpAddr>() {
        Ok(IpAddr::V6(v6)) => {
            v6.is_loopback() || v6.to_ipv4_mapped().is_some_and(|v4| v4.is_loopback())
        }
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::remote::parse_destination;

    use Locality::{Assumed, Local};

    const BIGMEM: &str = "devhost-devuser1-1a-e6904c1d.dc-west1.example.com";
    const HM: Kind = Kind::HomeManager;
    const DARWIN: Kind = Kind::Darwin;

    /// A host as [`local_hosts`] sees it, owning what [`Candidate`] borrows.
    struct Spec {
        name: &'static str,
        kind: Kind,
        destination: Option<Destination>,
        hostnames: Vec<String>,
    }

    fn spec(name: &'static str, kind: Kind, destination: Option<&str>) -> Spec {
        Spec {
            name,
            kind,
            destination: destination.map(|d| parse_destination(d).unwrap()),
            hostnames: Vec::new(),
        }
    }

    impl Spec {
        fn hostnames(mut self, names: &[&str]) -> Spec {
            self.hostnames = names.iter().map(|n| n.to_string()).collect();
            self
        }
    }

    fn localities(specs: &[Spec], local: &LocalInfo) -> Result<Vec<Locality>, String> {
        let candidates: Vec<Candidate<'_>> = specs
            .iter()
            .map(|s| Candidate {
                name: s.name,
                kind: s.kind,
                hostnames: &s.hostnames,
                destination: s.destination.as_ref(),
            })
            .collect();
        let localities = local_hosts(&candidates, local)?;
        assert_eq!(localities.len(), specs.len());
        Ok(localities)
    }

    /// The candidates that aren't remote.
    fn here(specs: &[Spec], local: &LocalInfo) -> Vec<(&'static str, Locality)> {
        let localities = localities(specs, local).unwrap_or_else(|e| panic!("{e}"));
        let both = specs.iter().zip(localities);
        both.filter(|(_, l)| *l != Locality::Remote)
            .map(|(s, l)| (s.name, l))
            .collect()
    }

    fn machine(hostname: &str, user: &str, os: Os) -> LocalInfo {
        LocalInfo {
            hostname: hostname.into(),
            user: user.into(),
            os,
            forced: None,
        }
    }

    fn on_bigmem() -> LocalInfo {
        machine(BIGMEM, "devuser1", Os::Linux)
    }

    fn on_the_mac() -> LocalInfo {
        machine("Julians-MacBook-Pro.local", "devuser1", Os::Darwin)
    }

    fn forced(mut local: LocalInfo, name: &str) -> LocalInfo {
        local.forced = Some(name.into());
        local
    }

    /// The user's hosts (nix-config's hosts/default.nix), reached as in
    /// examples/config.toml.
    fn real_hosts() -> Vec<Spec> {
        vec![
            spec(
                "al2-x86_64",
                HM,
                Some("devhost-devuser1-1a-8d41e07a.dc-west1.example.com"),
            ),
            spec(
                "al2-aarch64",
                HM,
                Some("devhost-devuser1-1a-7f3a0b82.dc-west1.example.com"),
            ),
            spec(
                "al2023-x86_64",
                HM,
                Some("devhost-devuser1-1a-c2d85e16.dc-west1.example.com"),
            ),
            spec("bigmem", HM, Some(BIGMEM)),
            spec("mac-m1", DARWIN, Some("ssh://devuser1@localhost:2022")),
        ]
    }

    #[test]
    fn on_bigmem_only_bigmem_is_local() {
        assert_eq!(here(&real_hosts(), &on_bigmem()), [("bigmem", Local)]);
        // By its short name too, and in any case.
        let short = machine("devhost-devuser1-1a-e6904c1d", "devuser1", Os::Linux);
        assert_eq!(here(&real_hosts(), &short), [("bigmem", Local)]);
        let upper = machine(&BIGMEM.to_uppercase(), "devuser1", Os::Linux);
        assert_eq!(here(&real_hosts(), &upper), [("bigmem", Local)]);
        // Another desk is that desk.
        let desk = machine(
            "devhost-devuser1-1a-8d41e07a.dc-west1.example.com",
            "devuser1",
            Os::Linux,
        );
        assert_eq!(here(&real_hosts(), &desk), [("al2-x86_64", Local)]);
    }

    #[test]
    fn ssh_aliases_need_hostnames() {
        // Destinations that are ssh-config aliases say nothing about the
        // machine's hostname; `hostnames` does.
        let specs = [
            spec("al2-x86_64", HM, Some("al2-x86_64"))
                .hostnames(&["devhost-devuser1-1a-8d41e07a.dc-west1.example.com"]),
            spec("bigmem", HM, Some("bigmem")).hostnames(&[BIGMEM]),
            spec("mac-m1", DARWIN, None),
        ];
        assert_eq!(here(&specs, &on_bigmem()), [("bigmem", Local)]);
        assert_eq!(
            here(&[spec("bigmem", HM, Some("bigmem"))], &on_bigmem()),
            []
        );
    }

    #[test]
    fn on_the_mac_its_only_darwin_host_is_assumed() {
        assert_eq!(here(&real_hosts(), &on_the_mac()), [("mac-m1", Assumed)]);
        // Unset destinations qualify too.
        let unset = [spec("mac-m1", DARWIN, None), spec("bigmem", HM, None)];
        assert_eq!(here(&unset, &on_the_mac()), [("mac-m1", Assumed)]);
        // Only on a Mac.
        let linux = machine("Julians-MacBook-Pro.local", "devuser1", Os::Linux);
        assert_eq!(here(&real_hosts(), &linux), []);
        let other = machine("Julians-MacBook-Pro.local", "devuser1", Os::Other);
        assert_eq!(here(&real_hosts(), &other), []);
        // Two candidates: no guess.
        let two = [
            spec("mac-m1", DARWIN, None),
            spec("mac-mini", DARWIN, Some("ssh://localhost:2023")),
        ];
        assert_eq!(here(&two, &on_the_mac()), []);
        // A Mac with a real destination is elsewhere, and no candidate.
        let elsewhere = [
            spec("mac-m1", DARWIN, Some("ssh://devuser1@localhost:2022")),
            spec("studio", DARWIN, Some("studio.lan")),
        ];
        assert_eq!(here(&elsewhere, &on_the_mac()), [("mac-m1", Assumed)]);
        // A real match leaves nothing to guess.
        let named = [
            spec("mac-m1", DARWIN, None),
            spec("julians-macbook-pro", DARWIN, None),
        ];
        assert_eq!(
            here(&named, &on_the_mac()),
            [("julians-macbook-pro", Local)]
        );
        // Non-darwin hosts are never assumed.
        assert_eq!(here(&[spec("box", HM, None)], &on_the_mac()), []);
    }

    #[test]
    fn self_forces_exactly_one_host() {
        let hosts = real_hosts();
        let wanted = forced(on_bigmem(), "al2-aarch64");
        assert_eq!(here(&hosts, &wanted), [("al2-aarch64", Local)]);
        // Whatever its route, and on the Mac without assuming.
        assert_eq!(
            here(&hosts, &forced(on_bigmem(), "mac-m1")),
            [("mac-m1", Local)]
        );
        assert_eq!(
            here(&hosts, &forced(on_the_mac(), "mac-m1")),
            [("mac-m1", Local)]
        );
        assert_eq!(
            localities(&hosts, &forced(on_bigmem(), "bigmen")),
            Err("unknown host \"bigmen\" in --self, HOSTCTL_SELF or self \
                 (hosts: al2-aarch64, al2-x86_64, al2023-x86_64, bigmem, mac-m1)"
                .into())
        );
        assert_eq!(
            localities(&[], &forced(on_bigmem(), "bigmem")),
            Err("unknown host \"bigmem\" in --self, HOSTCTL_SELF or self (hosts: none)".into())
        );
        let many: Vec<Spec> = [
            "h00", "h01", "h02", "h03", "h04", "h05", "h06", "h07", "h08", "h09", "h10",
        ]
        .iter()
        .rev()
        .map(|name| spec(name, HM, None))
        .collect();
        assert_eq!(
            localities(&many, &forced(on_bigmem(), "x\u{1b}")),
            Err("unknown host \"x\\u{1b}\" in --self, HOSTCTL_SELF or self \
                 (hosts: h00, h01, h02, h03, h04, h05, h06, h07, h08, h09, …)"
                .into())
        );
        let evil = [spec("evil\u{1b}]0;title\u{7}", HM, None)];
        assert_eq!(
            localities(&evil, &forced(on_bigmem(), "bigmem")),
            Err("unknown host \"bigmem\" in --self, HOSTCTL_SELF or self \
                 (hosts: evil\\u{1b}]0;title\\u{7})"
                .into())
        );
    }

    #[test]
    fn dotted_names_must_match_fully() {
        let prod = machine("web1.prod.example.com", "root", Os::Linux);
        let specs = [
            spec(
                "web1-staging",
                Kind::Nixos,
                Some("web1.staging.example.com"),
            ),
            spec("web1-prod", Kind::Nixos, Some("root@web1.prod.example.com")),
        ];
        assert_eq!(here(&specs, &prod), [("web1-prod", Local)]);
        // An explicit name that doesn't match vetoes the bare name.
        let vetoed = [
            spec("web1", Kind::Nixos, Some("web1.staging.example.com")),
            spec("web1", Kind::Nixos, None).hostnames(&["web1.staging.example.com"]),
            spec("web1", Kind::Nixos, Some("10.0.0.5")),
            spec("web1", Kind::Nixos, Some("ssh://[fe80::1]")),
        ];
        for spec in vetoed {
            assert_eq!(here(&[spec], &prod), []);
        }
        // Without one, the bare name counts (against the first label).
        assert_eq!(
            here(&[spec("web1", Kind::Nixos, None)], &prod),
            [("web1", Local)]
        );
        assert_eq!(
            here(&[spec("web1", Kind::Nixos, Some("web1-alias"))], &prod),
            [("web1", Local)]
        );
        let short = machine("web1", "root", Os::Linux);
        assert_eq!(
            here(&[spec("web1", Kind::Nixos, None)], &short),
            [("web1", Local)]
        );
        assert_eq!(here(&[spec("web2", Kind::Nixos, None)], &short), []);
    }

    #[test]
    fn another_user_is_another_machine() {
        let bob = machine("desk", "bob", Os::Linux);
        let specs = [
            spec("alice-desk", HM, Some("alice@desk")),
            spec("bob-desk", HM, Some("bob@desk")),
            spec("desk", HM, Some("desk")),
        ];
        assert_eq!(here(&specs, &bob), [("bob-desk", Local), ("desk", Local)]);
        // The veto covers hostnames too.
        let alice = [spec("alice-desk", HM, Some("alice@desk")).hostnames(&["desk"])];
        assert_eq!(here(&alice, &bob), []);
        // `user@host` names read like the destination they default to.
        let names = [
            spec("alice@desk", HM, None),
            spec("bob@desk", HM, None),
            spec("bob@laptop", HM, None),
        ];
        assert_eq!(here(&names, &bob), [("bob@desk", Local)]);
        // An unknown local user matches no user.
        let nobody = machine("desk", "", Os::Linux);
        assert_eq!(here(&specs, &nobody), [("desk", Local)]);
    }

    #[test]
    fn tunnels_and_forwarded_ports_are_other_machines() {
        let specs = [
            spec(
                "vm",
                Kind::Nixos,
                Some("ssh://devhost-devuser1-1a-e6904c1d.dc-west1.example.com:2222"),
            ),
            spec(
                "port-22",
                HM,
                Some("ssh://devhost-devuser1-1a-e6904c1d.dc-west1.example.com:22"),
            ),
            spec("loopback", HM, Some("localhost")).hostnames(&[BIGMEM]),
            spec("loopback6", HM, Some("ssh://[::1]")),
            spec("loopback4", HM, Some("devuser1@127.0.0.1")),
        ];
        assert_eq!(here(&specs, &on_bigmem()), [("port-22", Local)]);
    }

    #[test]
    fn hostnames_identify_the_machine() {
        let specs = [
            spec("mac", DARWIN, Some("mac-alias")).hostnames(&["julians-macbook-pro"]),
            spec("other", DARWIN, None).hostnames(&["other-mac"]),
        ];
        assert_eq!(here(&specs, &on_the_mac()), [("mac", Local)]);
        // Empty names never match.
        let empty = machine("", "devuser1", Os::Linux);
        assert_eq!(here(&[spec("x", HM, None).hostnames(&[""])], &empty), []);
    }

    #[test]
    fn same_host_compares_like_dns() {
        let same = [
            ("bigmem", "bigmem"),
            ("BigMem", "bigmem"),
            ("bigmem", "bigmem.example.com"),
            (BIGMEM, "devhost-devuser1-1a-e6904c1d"),
            (BIGMEM, "DEVHOST-DEVUSER1-1A-E6904C1D.DC-WEST1.EXAMPLE.COM"),
            ("bigmem.example.com.", "bigmem.example.com"),
            ("bigmem.", "bigmem"),
            ("Julians-MacBook-Pro.local", "julians-macbook-pro"),
            ("10.0.0.5", "10.0.0.5"),
            ("fe80::1", "fe80::1"),
        ];
        for (a, b) in same {
            assert!(same_host(a, b), "{a} ~ {b}");
            assert!(same_host(b, a), "{b} ~ {a}");
        }
        let different = [
            ("web1.prod.example.com", "web1.staging.example.com"),
            ("bigmem.example.com", "bigmem.example.org"),
            ("web1", "web2"),
            ("bigmem", "bigmem2.example.com"),
            (
                "devhost-devuser1-1a-e6904c1d",
                "devhost-devuser1-1a-8d41e07a.dc-west1.example.com",
            ),
            ("", ""),
            ("", "bigmem"),
            (".", "bigmem"),
            ("10", "10.0.0.5"),
            ("fe80", "fe80::1"),
        ];
        for (a, b) in different {
            assert!(!same_host(a, b), "{a} !~ {b}");
            assert!(!same_host(b, a), "{b} !~ {a}");
        }
    }

    #[test]
    fn loopback_hosts() {
        for host in [
            "localhost",
            "LocalHost",
            "localhost.",
            "localhost.localdomain",
            "127.0.0.1",
            "127.255.255.254",
            "127.1",
            "::1",
            "0:0:0:0:0:0:0:1",
            "[::1]",
            "::ffff:127.0.0.1",
        ] {
            assert!(is_loopback(host), "{host}");
        }
        for host in [
            "",
            "localhost2",
            "mylocalhost",
            "localhost.example.com",
            "128.0.0.1",
            "1.127.0.0",
            "127.",
            "127.x",
            "::2",
            "::",
            "0.0.0.0",
            "[::1",
            "bigmem",
            BIGMEM,
        ] {
            assert!(!is_loopback(host), "{host}");
        }
    }

    #[test]
    fn detects_this_machine() {
        let info = LocalInfo::detect(None);
        assert_eq!(info.os, Os::current());
        assert_eq!(info.forced, None);
        // gethostname(2) works everywhere this runs (the Nix sandbox says
        // "localhost").
        assert_eq!(Some(&info.hostname), os_hostname().as_ref());
        if let Some(user) = env_value("USER") {
            assert_eq!(info.user, user);
        }
        let forced = LocalInfo::detect(Some("bigmem".into()));
        assert_eq!(forced.forced.as_deref(), Some("bigmem"));
        assert_eq!(LocalInfo::detect(Some(String::new())).forced, None);
    }

    #[test]
    fn detection_falls_back_in_order() {
        let some = |s: &str| Some(s.to_owned());
        assert_eq!(pick_hostname(some("bigmem"), || some("from-env")), "bigmem");
        assert_eq!(pick_hostname(None, || some("from-env")), "from-env");
        assert_eq!(pick_hostname(None, || None), "localhost");
        let unused = || -> Option<String> { panic!("looked up passwd although $USER is set") };
        assert_eq!(pick_user(some("devuser1"), unused), "devuser1");
        assert_eq!(pick_user(None, || some("nixbld")), "nixbld");
        assert_eq!(pick_user(None, || None), "");
    }

    #[test]
    fn gethostname_buffers_are_read_up_to_the_nul() {
        assert_eq!(c_name(b"bigmem\0garbage").as_deref(), Some("bigmem"));
        // Truncated names may lack the NUL.
        assert_eq!(c_name(b"bigmem").as_deref(), Some("bigmem"));
        assert_eq!(
            c_name(b"caf\xc3\xa9-\xff\0").as_deref(),
            Some("café-\u{fffd}")
        );
        assert_eq!(c_name(b"\0bigmem"), None);
        assert_eq!(c_name(b""), None);
    }

    #[test]
    fn hostname_comes_from_the_kernel() {
        let name = os_hostname().expect("gethostname(2) works");
        assert!(!name.contains('\0'));
        #[cfg(target_os = "linux")]
        if let Ok(kernel) = std::fs::read_to_string("/proc/sys/kernel/hostname") {
            assert_eq!(name, kernel.trim_end_matches('\n'));
        }
    }

    #[test]
    fn passwd_names_the_effective_user() {
        match passwd_user() {
            Some(name) => assert!(
                !name.is_empty() && !name.contains([':', '\n', '\0']),
                "{name:?}"
            ),
            // Containers may run as a uid without an entry.
            None => eprintln!("no passwd entry for uid {}", unsafe { libc::geteuid() }),
        }
    }
}
