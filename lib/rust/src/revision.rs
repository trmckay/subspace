use std::fmt;

use git2::Oid;
use serde::{Deserialize, Deserializer, Serialize, Serializer};

use crate::error::{Error, Result};

/// A git object that resolves to a commit, such as a SHA or a branch.
pub trait Revision: fmt::Display {
    /// Returns the full hex SHA string.
    fn sha(&self) -> String;
}

/// A bare commit SHA.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Sha(Oid);

impl Sha {
    pub(crate) fn new(oid: Oid) -> Sha {
        Sha(oid)
    }

    /// Parses a hex commit SHA. Fails with [`Error::InvalidSha`] on invalid hex.
    pub fn from_hex(hex: &str) -> Result<Sha> {
        Oid::from_str(hex)
            .map(Sha)
            .map_err(|_| Error::InvalidSha(hex.to_string()))
    }
}

impl Serialize for Sha {
    fn serialize<S>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(&self.0.to_string())
    }
}

impl<'de> Deserialize<'de> for Sha {
    fn deserialize<D>(deserializer: D) -> std::result::Result<Sha, D::Error>
    where
        D: Deserializer<'de>,
    {
        let hex = String::deserialize(deserializer)?;
        Sha::from_hex(&hex).map_err(serde::de::Error::custom)
    }
}

impl Revision for Sha {
    fn sha(&self) -> String {
        self.0.to_string()
    }
}

impl fmt::Display for Sha {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let hex = self.0.to_string();
        write!(f, "{}", &hex[..8.min(hex.len())])
    }
}

/// A branch reference (name + commit SHA).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Branch {
    name: String,
    sha: Sha,
}

impl Branch {
    pub(crate) fn new(name: String, sha: Sha) -> Branch {
        Branch { name, sha }
    }

    /// Returns the branch name.
    pub fn name(&self) -> &str {
        &self.name
    }
}

impl Revision for Branch {
    fn sha(&self) -> String {
        self.sha.sha()
    }
}

impl fmt::Display for Branch {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.name)
    }
}

/// What HEAD points to: either a branch or a detached SHA.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Head {
    /// HEAD points to a bare SHA (no branch).
    Detached(Sha),
    /// HEAD points to a branch.
    Branch(Branch),
}

impl Head {
    /// Returns the branch if HEAD is on one, `None` if detached.
    pub fn branch(&self) -> Option<&Branch> {
        match self {
            Head::Branch(branch) => Some(branch),
            Head::Detached(_) => None,
        }
    }

    /// Returns the commit HEAD points to as a [`Sha`].
    pub fn commit(&self) -> Sha {
        match self {
            Head::Detached(sha) => *sha,
            Head::Branch(branch) => branch.sha,
        }
    }
}

impl Revision for Head {
    fn sha(&self) -> String {
        match self {
            Head::Detached(sha) => sha.sha(),
            Head::Branch(branch) => branch.sha(),
        }
    }
}

impl fmt::Display for Head {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Head::Detached(sha) => write!(f, "{}", sha),
            Head::Branch(branch) => write!(f, "{}", branch),
        }
    }
}

/// The resolved starting point for a new workspace.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StartPoint {
    /// An existing local branch, checked out directly.
    Branch(Branch),
    /// An existing remote-tracking branch, for example `origin/feature`. A new local branch is
    /// created to track it.
    RemoteBranch(Branch),
    /// A bare commit (tag, SHA, or a workspace's resolved HEAD).
    Commit(Sha),
}

impl Revision for StartPoint {
    fn sha(&self) -> String {
        match self {
            StartPoint::Branch(branch) | StartPoint::RemoteBranch(branch) => branch.sha(),
            StartPoint::Commit(sha) => sha.sha(),
        }
    }
}

impl fmt::Display for StartPoint {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            StartPoint::Branch(branch) | StartPoint::RemoteBranch(branch) => {
                write!(f, "{}", branch)
            }
            StartPoint::Commit(sha) => write!(f, "{}", sha),
        }
    }
}

/// A binary git patch produced by `git diff --binary`.
#[derive(Debug, Clone)]
pub struct BinaryPatch(Vec<u8>);

impl BinaryPatch {
    pub(crate) fn new(bytes: Vec<u8>) -> BinaryPatch {
        BinaryPatch(bytes)
    }

    pub(crate) fn as_bytes(&self) -> &[u8] {
        &self.0
    }

    /// Returns whether the patch contains no changes.
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}
