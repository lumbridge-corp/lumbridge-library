//! Who is who in one library, from the `[identity]` table of the daemon's config.
//!
//! - `owner`: the person the library belongs to. Documents with `author: <owner>` (and every
//!   document that does not name an agent author) are never edited by agents. Admin actions are
//!   logged as `<owner>@<client>`, and notes without an author in the owner's own layers (note, thought, daily) count as theirs.
//! - `agents`: the author names agents may write under. Only files whose frontmatter names one of
//!   them (or the librarian) as the single author can be rewritten.
//! - `librarian`: the author name the librarian writes under; it counts as an agent.
//! - `git_name` / `git_email`: the identity of every commit the daemon makes (vault, component and
//!   design repos). The vault pull job tells the daemon's own commits from other people's by name.
//! - `design_license`: the SPDX id recorded for your own design write-ups.
//!
//! The daemon calls [`set`] once at start-up. Code that runs without it (tests, examples) sees
//! [`Identity::default`].

use serde::Deserialize;
use std::sync::OnceLock;

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Identity {
    #[serde(default = "default_owner")]
    pub owner: String,
    #[serde(default = "default_agents")]
    pub agents: Vec<String>,
    #[serde(default = "default_librarian")]
    pub librarian: String,
    #[serde(default = "default_git_name")]
    pub git_name: String,
    #[serde(default = "default_git_email")]
    pub git_email: String,
    #[serde(default = "default_design_license")]
    pub design_license: String,
}

impl Default for Identity {
    fn default() -> Self {
        Self {
            owner: default_owner(),
            agents: default_agents(),
            librarian: default_librarian(),
            git_name: default_git_name(),
            git_email: default_git_email(),
            design_license: default_design_license(),
        }
    }
}

fn default_owner() -> String {
    "owner".into()
}
fn default_agents() -> Vec<String> {
    vec!["agent".into(), "claude".into()]
}
fn default_librarian() -> String {
    "librarian".into()
}
fn default_git_name() -> String {
    "lumbridge-library".into()
}
fn default_git_email() -> String {
    "lumbridge-library@localhost".into()
}
fn default_design_license() -> String {
    "LicenseRef-private".into()
}

impl Identity {
    /// Lower-cased, trimmed, empty agent names dropped, the owner never among the agents.
    pub fn normalized(mut self) -> Self {
        self.owner = self.owner.trim().to_lowercase();
        let owner = self.owner.clone();
        self.agents = self
            .agents
            .iter()
            .map(|a| a.trim().to_lowercase())
            .filter(|a| !a.is_empty() && *a != owner)
            .collect();
        self.agents.dedup();
        self.librarian = self.librarian.trim().to_lowercase();
        if self.librarian.is_empty() || self.librarian == owner {
            self.librarian = default_librarian();
        }
        self
    }

    pub fn is_agent(&self, name: &str) -> bool {
        let n = name.trim().to_lowercase();
        n == self.librarian || self.agents.contains(&n)
    }

    pub fn is_owner(&self, name: &str) -> bool {
        name.trim().eq_ignore_ascii_case(&self.owner)
    }

    /// The agent author used when a caller does not say which agent it is.
    pub fn default_agent(&self) -> &str {
        self.agents.first().map(String::as_str).unwrap_or("agent")
    }

    /// `"a, b or c"` for messages (the agents, then the librarian).
    pub fn agents_list(&self) -> String {
        let mut all = self.agents.clone();
        if !all.contains(&self.librarian) {
            all.push(self.librarian.clone());
        }
        match all.as_slice() {
            [] => "(none configured)".into(),
            [one] => one.clone(),
            [rest @ .., last] => format!("{} or {last}", rest.join(", ")),
        }
    }
}

static IDENTITY: OnceLock<Identity> = OnceLock::new();

/// Set the identity for this process. The first call wins; later calls are ignored.
pub fn set(id: Identity) {
    let _ = IDENTITY.set(id.normalized());
}

/// The identity set at start-up, else the defaults.
pub fn get() -> &'static Identity {
    IDENTITY.get_or_init(|| Identity::default().normalized())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalized_drops_owner_and_blanks() {
        let id = Identity { owner: " Sam ".into(), agents: vec!["Helper".into(), "sam".into(), " ".into()], ..Default::default() }.normalized();
        assert_eq!(id.owner, "sam");
        assert_eq!(id.agents, vec!["helper".to_string()]);
        assert!(id.is_agent("HELPER") && !id.is_agent("sam") && id.is_owner("SAM") && id.is_agent("librarian"));
        assert_eq!(id.default_agent(), "helper");
    }

    #[test]
    fn parses_partial_table() {
        let id: Identity = toml::from_str("owner = \"sam\"\nagents = [\"a\", \"b\", \"c\"]").unwrap();
        assert_eq!(id.git_name, "lumbridge-library");
        assert_eq!(id.normalized().agents_list(), "a, b, c or librarian");
    }
}
