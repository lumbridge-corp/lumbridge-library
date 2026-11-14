//! Names Lumbridge Library has used, in one place.
//!
//! Two kinds of name:
//!
//! 1. **Current names with earlier fallbacks** (environment variables, config folder, config file,
//!    data root, log targets). The current name wins; the name an earlier release used is still
//!    read when the current one is absent, so an existing install keeps working after an upgrade.
//!    Nothing on disk is ever moved, copied or rewritten: the code only chooses which existing path
//!    to read.
//! 2. **Fixed wire names** ([`wire`]): names other programs, running agents or stored data key on
//!    (HTTP headers, the MCP server name, actor and trace tags). They are constants here so that
//!    the day one of them changes, the compiler shows every user.
//!
//! No dependencies on purpose: the daemon, the engine crate and the thin CLIs all use it.

use std::ffi::OsString;
use std::path::{Path, PathBuf};

/// One environment variable: its name now, and the names it had before (in fallback order).
#[derive(Debug, Clone, Copy)]
pub struct EnvName {
    pub new: &'static str,
    pub old: &'static [&'static str],
}

/// Base URL of the daemon, for the CLIs and tools. Default: [`DEFAULT_URL`].
pub const ENV_URL: EnvName = EnvName { new: "LUMBRIDGE_LIBRARY_URL", old: &["LIBRARY_URL"] };
/// Bearer token for the CLIs (else the `token` file in the config folder).
pub const ENV_TOKEN: EnvName = EnvName { new: "LUMBRIDGE_LIBRARY_TOKEN", old: &["LIBRARY_TOKEN"] };
/// The owner's admin token for the CLIs (else the `admin-token` file in the config folder).
pub const ENV_ADMIN_TOKEN: EnvName = EnvName { new: "LUMBRIDGE_LIBRARY_ADMIN_TOKEN", old: &["LIBRARY_ADMIN_TOKEN"] };
/// Path of the daemon's config file (same as `--config`).
pub const ENV_DAEMON_CONFIG: EnvName = EnvName { new: "LUMBRIDGE_LIBRARYD_CONFIG", old: &["LIBRARYD_CONFIG"] };

/// Where the CLIs look for the daemon when `LUMBRIDGE_LIBRARY_URL` is not set.
pub const DEFAULT_URL: &str = "http://127.0.0.1:8830";

/// Config folder under `$HOME`: current, then the one earlier releases used.
pub const CONFIG_DIR: &str = ".config/lumbridge-library";
pub const OLD_CONFIG_DIR: &str = ".config/library";
/// Daemon config file name: current, then earlier. Same TOML format.
pub const CONFIG_FILE: &str = "lumbridge-libraryd.toml";
pub const OLD_CONFIG_FILE: &str = "libraryd.toml";
/// Default data root under `$HOME` (vault/, warehouse/, attachments/, datasets/) when the config
/// does not set `library_root`.
pub const DATA_ROOT: &str = "lumbridge-library";
pub const OLD_DATA_ROOT: &str = "library";

/// Log targets (Rust crate names) of earlier releases, and what they are now. A `RUST_LOG` written
/// for the earlier names keeps showing the same lines.
pub const LOG_TARGETS: &[(&str, &str)] = &[("libraryd", "lumbridge_libraryd"), ("library_core", "lumbridge_library_core")];
/// The daemon's log filter when `RUST_LOG` is not set.
pub const DEFAULT_LOG_FILTER: &str = "lumbridge_libraryd=info,lumbridge_library_core=info";

/// Names that are a contract with something outside this repo.
pub mod wire {
    /// Admin token header on owner-only REST routes. Sent by the CLIs and other trusted clients.
    pub const HEADER_ADMIN: &str = "x-library-admin";
    /// Who is calling a REST route ("cli", an app name, ...). Stored as the actor in `lib.ops_log`.
    pub const HEADER_CLIENT: &str = "x-library-client";
    /// `serverInfo.name` in the MCP initialize answer. (The name agents see in tool ids, such as
    /// `mcp__library__*`, is set in each client's own config, not here.)
    pub const MCP_SERVER_NAME: &str = "lumbridge-library";
    /// The daemon as an actor in `lib.ops_log` and in job lists (the activity feed shows it).
    pub const ACTOR: &str = "lumbridge-libraryd";
    /// `x-tracekit-client` tags on model calls, so a tracing proxy keeps one history per caller.
    pub const TRACE_LIBRARIAN: &str = "lumbridge-library-librarian";
    pub const TRACE_DESIGNS: &str = "lumbridge-library-designs";
}

/// The value of an environment variable under its current name, else under the first earlier name
/// that is set. Empty counts as unset.
pub fn env(name: EnvName) -> Option<String> {
    env_with(name, |k| std::env::var_os(k))
}

/// [`env`] with the lookup passed in (tests never touch the process environment).
pub fn env_with(name: EnvName, get: impl Fn(&str) -> Option<OsString>) -> Option<String> {
    std::iter::once(name.new)
        .chain(name.old.iter().copied())
        .filter_map(get)
        .filter_map(|v| v.into_string().ok())
        .find(|v| !v.is_empty())
}

/// `<home>/.config/lumbridge-library` if it exists, else `<home>/.config/library` if that exists,
/// else the current one (for messages and fresh installs).
pub fn config_dir(home: &Path) -> PathBuf {
    first_existing([home.join(CONFIG_DIR), home.join(OLD_CONFIG_DIR)])
}

/// A file the CLIs read from the config folder (`token`, `admin-token`): the current folder's copy
/// if it exists, else the earlier folder's, else the current path.
pub fn config_file(home: &Path, name: &str) -> PathBuf {
    first_existing([home.join(CONFIG_DIR).join(name), home.join(OLD_CONFIG_DIR).join(name)])
}

/// The daemon's config file when neither `--config` nor the environment names one:
/// `lumbridge-libraryd.toml` in the current folder, else the earlier file name in the current
/// folder, else the earlier file name in the earlier folder, else the first.
pub fn daemon_config(home: &Path) -> PathBuf {
    first_existing([
        home.join(CONFIG_DIR).join(CONFIG_FILE),
        home.join(CONFIG_DIR).join(OLD_CONFIG_FILE),
        home.join(OLD_CONFIG_DIR).join(OLD_CONFIG_FILE),
    ])
}

/// Default data root: `<home>/lumbridge-library`, unless only `<home>/library` exists. The folder
/// is only ever chosen, never created, moved or copied here.
pub fn data_root(home: &Path) -> PathBuf {
    first_existing([home.join(DATA_ROOT), home.join(OLD_DATA_ROOT)])
}

fn first_existing<const N: usize>(candidates: [PathBuf; N]) -> PathBuf {
    let mut it = candidates.into_iter();
    let first = it.next().expect("at least one candidate");
    if first.exists() {
        return first;
    }
    it.find(|p| p.exists()).unwrap_or(first)
}

/// A `RUST_LOG` value that also covers the current crate names: every directive for an earlier log target
/// (`libraryd=info`, `library_core::docs=debug`) gets a twin for the new target. Everything else is
/// passed through untouched, so a filter written for the new names is returned as it came.
pub fn log_filter(spec: &str) -> String {
    let directives: Vec<&str> = spec.split(',').map(str::trim).filter(|d| !d.is_empty()).collect();
    let target_of = |d: &str| d.split_once('=').map_or(d, |(t, _)| t).to_string();
    // A new target the caller already set is theirs to decide; it gets no twin.
    let explicit: Vec<String> = directives.iter().map(|d| target_of(d)).collect();
    let mut out: Vec<String> = Vec::new();
    for directive in directives {
        out.push(directive.to_string());
        let (target, level) = match directive.split_once('=') {
            Some((t, l)) => (t, Some(l)),
            None => (directive, None),
        };
        for (old, new) in LOG_TARGETS {
            let rest = match target.strip_prefix(old) {
                Some(r) if r.is_empty() || r.starts_with("::") => r,
                _ => continue,
            };
            if explicit.contains(&format!("{new}{rest}")) {
                continue;
            }
            out.push(match level {
                Some(l) => format!("{new}{rest}={l}"),
                None => format!("{new}{rest}"),
            });
        }
    }
    out.join(",")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn lookup(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<OsString> {
        let m: HashMap<String, OsString> = pairs.iter().map(|(k, v)| (k.to_string(), OsString::from(v))).collect();
        move |k| m.get(k).cloned()
    }

    #[test]
    fn env_new_name_wins_then_old_and_empty_is_unset() {
        assert_eq!(env_with(ENV_URL, lookup(&[])), None);
        assert_eq!(env_with(ENV_URL, lookup(&[("LIBRARY_URL", "http://old")])).as_deref(), Some("http://old"));
        assert_eq!(
            env_with(ENV_URL, lookup(&[("LIBRARY_URL", "http://old"), ("LUMBRIDGE_LIBRARY_URL", "http://new")])).as_deref(),
            Some("http://new")
        );
        assert_eq!(
            env_with(ENV_URL, lookup(&[("LIBRARY_URL", "http://old"), ("LUMBRIDGE_LIBRARY_URL", "")])).as_deref(),
            Some("http://old")
        );
        assert_eq!(env_with(ENV_TOKEN, lookup(&[("LIBRARY_TOKEN", "t")])).as_deref(), Some("t"));
        assert_eq!(env_with(ENV_ADMIN_TOKEN, lookup(&[("LIBRARY_ADMIN_TOKEN", "a")])).as_deref(), Some("a"));
        assert_eq!(env_with(ENV_DAEMON_CONFIG, lookup(&[("LIBRARYD_CONFIG", "/x.toml")])).as_deref(), Some("/x.toml"));
    }

    #[test]
    fn earlier_install_layout_is_found_unchanged() {
        // An install made by an earlier release: only ~/.config/library and ~/library.
        let home = tempfile::tempdir().unwrap();
        let h = home.path();
        std::fs::create_dir_all(h.join(".config/library")).unwrap();
        std::fs::create_dir_all(h.join("library")).unwrap();
        std::fs::write(h.join(".config/library/libraryd.toml"), "").unwrap();
        std::fs::write(h.join(".config/library/token"), "t").unwrap();
        assert_eq!(config_dir(h), h.join(".config/library"));
        assert_eq!(daemon_config(h), h.join(".config/library/libraryd.toml"));
        assert_eq!(config_file(h, "token"), h.join(".config/library/token"));
        assert_eq!(data_root(h), h.join("library"));
        // A file the old folder does not have is reported at the new path.
        assert_eq!(config_file(h, "admin-token"), h.join(".config/lumbridge-library/admin-token"));
    }

    #[test]
    fn new_layout_wins_and_is_the_default_on_an_empty_home() {
        let home = tempfile::tempdir().unwrap();
        let h = home.path();
        assert_eq!(config_dir(h), h.join(".config/lumbridge-library"));
        assert_eq!(daemon_config(h), h.join(".config/lumbridge-library/lumbridge-libraryd.toml"));
        assert_eq!(data_root(h), h.join("lumbridge-library"));

        for d in [".config/library", ".config/lumbridge-library", "library", "lumbridge-library"] {
            std::fs::create_dir_all(h.join(d)).unwrap();
        }
        std::fs::write(h.join(".config/library/libraryd.toml"), "").unwrap();
        std::fs::write(h.join(".config/library/token"), "old").unwrap();
        // New folder exists but holds no config yet: the old file is still the one in use.
        assert_eq!(daemon_config(h), h.join(".config/library/libraryd.toml"));
        assert_eq!(config_file(h, "token"), h.join(".config/library/token"));
        std::fs::write(h.join(".config/lumbridge-library/libraryd.toml"), "").unwrap();
        assert_eq!(daemon_config(h), h.join(".config/lumbridge-library/libraryd.toml"));
        std::fs::write(h.join(".config/lumbridge-library/lumbridge-libraryd.toml"), "").unwrap();
        std::fs::write(h.join(".config/lumbridge-library/token"), "new").unwrap();
        assert_eq!(daemon_config(h), h.join(".config/lumbridge-library/lumbridge-libraryd.toml"));
        assert_eq!(config_file(h, "token"), h.join(".config/lumbridge-library/token"));
        assert_eq!(config_dir(h), h.join(".config/lumbridge-library"));
        assert_eq!(data_root(h), h.join("lumbridge-library"));
        // Nothing was moved or removed by looking.
        assert!(h.join(".config/library/libraryd.toml").exists() && h.join("library").exists());
    }

    #[test]
    fn old_log_targets_get_a_twin() {
        // The value units from earlier releases set.
        assert_eq!(
            log_filter("libraryd=info,library_core=info"),
            "libraryd=info,lumbridge_libraryd=info,library_core=info,lumbridge_library_core=info"
        );
        assert_eq!(log_filter("library_core::docs=debug,warn"), "library_core::docs=debug,lumbridge_library_core::docs=debug,warn");
        assert_eq!(log_filter("libraryd"), "libraryd,lumbridge_libraryd");
        // New names, other crates and look-alikes pass through.
        assert_eq!(log_filter(DEFAULT_LOG_FILTER), DEFAULT_LOG_FILTER);
        assert_eq!(log_filter("libraryd_extra=info,sqlx=warn, debug"), "libraryd_extra=info,sqlx=warn,debug");
        assert_eq!(log_filter("libraryd=info,lumbridge_libraryd=debug"), "libraryd=info,lumbridge_libraryd=debug");
    }
}
