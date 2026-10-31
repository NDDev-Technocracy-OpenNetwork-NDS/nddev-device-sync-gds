//! GitHub accounts the owner decided this module never discovers, connects or synchronizes.
//!
//! The list uses the Estate `excluded-accounts.yaml` schema (`schema_version: 1`,
//! `excluded_accounts[]` with `login`, `provider`, `kind` and descriptive fields). A host
//! installs it at [`default_path`]; an absent file means no ignored account. A present file that
//! cannot be read or validated fails closed: callers refuse the operation instead of including
//! anything. An entry may pin the immutable numeric account `id`; it then matches by id only,
//! because GitHub logins can be renamed and later claimed by another account.
use serde::Deserialize;
use std::path::{Path, PathBuf};

/// File name of the list, equal to the Estate source it is installed from.
pub const FILE_NAME: &str = "excluded-accounts.yaml";
const MAX_BYTES: u64 = 64 * 1024;
const MAX_ENTRIES: usize = 256;

/// The list cannot be read or validated; nothing may be included on its account.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct InvalidList;

/// How the list treats one GitHub account.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Decision {
    Included,
    Ignored,
    /// Ignored by its pinned id although its login changed; the list should be refreshed.
    IgnoredRenamed,
}

impl Decision {
    pub fn ignored(self) -> bool {
        !matches!(self, Self::Included)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct Entry {
    /// ASCII-lowercased; GitHub logins compare case-insensitively.
    login: String,
    id: Option<u64>,
}

/// A validated set of ignored GitHub accounts.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct IgnoredAccounts {
    entries: Vec<Entry>,
}

#[derive(Deserialize)]
struct ListWire {
    schema_version: u32,
    #[serde(default)]
    excluded_accounts: Vec<EntryWire>,
}

// Descriptive fields (reason, decided_by, decided_at, ...) belong to the Estate record and are
// not interpreted here.
#[derive(Deserialize)]
struct EntryWire {
    login: String,
    provider: String,
    kind: String,
    #[serde(default)]
    id: Option<u64>,
}

fn valid_login(login: &str) -> bool {
    (1..=39).contains(&login.len())
        && !login.starts_with('-')
        && login
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-')
}

impl IgnoredAccounts {
    /// Validates the YAML list. Entries of another provider are not this module's and are
    /// skipped; a GitHub entry must name an organization or a user.
    pub fn parse(text: &str) -> Result<Self, InvalidList> {
        let options = serde_saphyr::options! {
            budget: serde_saphyr::budget! {
                max_documents: 1,
                max_depth: 8,
                max_nodes: 8192,
                max_aliases: 0,
                max_anchors: 0,
                max_total_scalar_bytes: MAX_BYTES as usize,
            },
        };
        let wire: ListWire =
            serde_saphyr::from_str_with_options(text, options).map_err(|_| InvalidList)?;
        if wire.schema_version != 1 || wire.excluded_accounts.len() > MAX_ENTRIES {
            return Err(InvalidList);
        }
        let mut entries: Vec<Entry> = Vec::new();
        for entry in wire.excluded_accounts {
            if entry.provider.is_empty() || entry.provider.chars().any(char::is_control) {
                return Err(InvalidList);
            }
            if entry.provider != "github" {
                continue;
            }
            if !matches!(entry.kind.as_str(), "organization" | "user")
                || !valid_login(&entry.login)
                || entry.id == Some(0)
            {
                return Err(InvalidList);
            }
            let login = entry.login.to_ascii_lowercase();
            if entries
                .iter()
                .any(|e| e.login == login || entry.id.is_some() && e.id == entry.id)
            {
                return Err(InvalidList);
            }
            entries.push(Entry {
                login,
                id: entry.id,
            });
        }
        Ok(Self { entries })
    }

    /// Reads the list at `path`. An absent file is an empty list; anything else that is not a
    /// readable regular file of at most 64 KiB with a valid list fails closed.
    pub fn read(path: &Path) -> Result<Self, InvalidList> {
        let metadata = match std::fs::metadata(path) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok(Self::default());
            }
            Err(_) => return Err(InvalidList),
        };
        if !metadata.is_file() || metadata.len() > MAX_BYTES {
            return Err(InvalidList);
        }
        let text = std::fs::read_to_string(path).map_err(|_| InvalidList)?;
        if text.len() as u64 > MAX_BYTES {
            return Err(InvalidList);
        }
        Self::parse(&text)
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Decides one account by its immutable `id` and current `login`.
    pub fn decide(&self, id: u64, login: &str) -> Decision {
        let login = login.to_ascii_lowercase();
        for entry in &self.entries {
            match entry.id {
                Some(pinned) if pinned == id => {
                    return if entry.login == login {
                        Decision::Ignored
                    } else {
                        Decision::IgnoredRenamed
                    };
                }
                Some(_) => {}
                None if entry.login == login => return Decision::Ignored,
                None => {}
            }
        }
        Decision::Included
    }
}

/// Where a host installs the list: `$XDG_CONFIG_HOME/nds/github/` (or `~/.config/nds/github/`)
/// on Linux and macOS, `%APPDATA%\nds\github\` on Windows. `None` without a home directory.
pub fn default_path() -> Option<PathBuf> {
    let absolute = |name: &str| {
        std::env::var_os(name)
            .map(PathBuf::from)
            .filter(|path| path.is_absolute())
    };
    let base = if cfg!(windows) {
        absolute("APPDATA")?
    } else {
        absolute("XDG_CONFIG_HOME").or_else(|| absolute("HOME").map(|home| home.join(".config")))?
    };
    Some(base.join("nds").join("github").join(FILE_NAME))
}

/// The list a runtime consults. Each operation reads it again, so an edit applies to the next
/// discovery, connection or synchronization without a restart.
#[derive(Debug, Default)]
pub struct Source {
    path: std::sync::Mutex<Option<PathBuf>>,
}

impl Source {
    pub fn at(path: Option<PathBuf>) -> Self {
        Self {
            path: std::sync::Mutex::new(path),
        }
    }

    pub fn host_default() -> Self {
        Self::at(default_path())
    }

    pub fn set(&self, path: Option<PathBuf>) {
        *self.path.lock().unwrap_or_else(|error| error.into_inner()) = path;
    }

    pub fn current(&self) -> Result<IgnoredAccounts, InvalidList> {
        let path = self
            .path
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .clone();
        match path {
            Some(path) => IgnoredAccounts::read(&path),
            None => Ok(IgnoredAccounts::default()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const ESTATE: &str = r#"schema_version: 1

# Accounts the authenticated identity can reach but the estate must never register.
excluded_accounts:
  - login: "mysummit-school"
    provider: "github"
    kind: "organization"
    reason: >-
      A colleague's project.
    decided_by: "owner"
    decided_at: "2026-09-11"
    extended_at: "2026-10-31"
  - login: "NDDev-it-com"
    provider: "github"
    kind: "organization"
    reason: >-
      Legacy organization.
    decided_by: "owner"
    decided_at: "2026-10-31"
  - login: "pinned-org"
    id: 42
    provider: "github"
    kind: "organization"
    reason: "pinned"
    decided_by: "owner"
    decided_at: "2026-11-01"
  - login: "someone"
    provider: "gitlab"
    kind: "group"
    reason: "not this module"
    decided_by: "owner"
    decided_at: "2026-11-01"
"#;

    #[test]
    fn the_estate_list_ignores_by_login_case_insensitively_and_by_pinned_id() {
        let list = IgnoredAccounts::parse(ESTATE).unwrap();
        assert_eq!(list.decide(7, "mysummit-school"), Decision::Ignored);
        assert_eq!(list.decide(8, "nddev-IT-com"), Decision::Ignored);
        assert_eq!(list.decide(9, "nddev"), Decision::Included);
        // A pinned id follows the account through a rename, and its old login
        // no longer ignores whoever claims it next.
        assert_eq!(list.decide(42, "pinned-org"), Decision::Ignored);
        assert_eq!(list.decide(42, "renamed-org"), Decision::IgnoredRenamed);
        assert_eq!(list.decide(43, "pinned-org"), Decision::Included);
        // Another provider's entry is not this module's.
        assert_eq!(list.decide(10, "someone"), Decision::Included);
    }

    #[test]
    fn a_malformed_list_fails_closed() {
        for text in [
            "schema_version: 2\nexcluded_accounts: []\n",
            "excluded_accounts: []\n",
            "schema_version: 1\nexcluded_accounts:\n  - login: \"-bad\"\n    provider: github\n    kind: organization\n",
            "schema_version: 1\nexcluded_accounts:\n  - login: a\n    provider: github\n    kind: team\n",
            "schema_version: 1\nexcluded_accounts:\n  - login: a\n    provider: github\n    kind: user\n    id: 0\n",
            "schema_version: 1\nexcluded_accounts:\n  - login: a\n    provider: github\n    kind: user\n  - login: A\n    provider: github\n    kind: user\n",
            "schema_version: 1\nexcluded_accounts:\n  - login: a\n    provider: github\n    kind: user\n    id: 5\n  - login: b\n    provider: github\n    kind: user\n    id: 5\n",
            "schema_version: 1\nexcluded_accounts:\n  - &x {login: a, provider: github, kind: user}\n  - *x\n",
            "schema_version: 1\nschema_version: 1\n",
            "schema_version: 1\nexcluded_accounts:\n  - login: a\n    kind: user\n",
            "[",
        ] {
            assert_eq!(IgnoredAccounts::parse(text), Err(InvalidList), "{text}");
        }
    }

    #[test]
    fn an_absent_file_ignores_nothing_and_an_unreadable_one_fails_closed() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join(FILE_NAME);
        let source = Source::at(Some(path.clone()));
        assert!(source.current().unwrap().is_empty());
        std::fs::write(&path, ESTATE).unwrap();
        assert_eq!(
            source.current().unwrap().decide(1, "MySummit-School"),
            Decision::Ignored
        );
        std::fs::write(&path, "schema_version: [").unwrap();
        assert_eq!(source.current(), Err(InvalidList));
        std::fs::remove_file(&path).unwrap();
        std::fs::create_dir(&path).unwrap();
        assert_eq!(source.current(), Err(InvalidList));
        std::fs::remove_dir(&path).unwrap();
        std::fs::write(&path, vec![b'#'; MAX_BYTES as usize + 1]).unwrap();
        assert_eq!(source.current(), Err(InvalidList));
        assert!(Source::at(None).current().unwrap().is_empty());
    }
}
