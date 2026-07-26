//! Hierarchical agent addresses: `/survey/parser`.

use std::fmt;

#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct AgentPath(String);

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum PathError {
    #[error("agent name is empty")]
    Empty,
    #[error("'{0}' is not a usable agent name — use letters, digits, '-' or '_'")]
    Invalid(String),
    #[error("'{0}' has no parent — it is the root")]
    NoParent(String),
}

pub const ROOT: &str = "/";

impl AgentPath {
    pub fn root() -> Self {
        Self(ROOT.to_string())
    }

    pub fn is_root(&self) -> bool {
        self.0 == ROOT
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    pub fn name(&self) -> &str {
        match self.0.rsplit_once('/') {
            Some((_, name)) if !name.is_empty() => name,
            _ => ROOT,
        }
    }

    pub fn depth(&self) -> u32 {
        if self.is_root() {
            return 0;
        }
        self.0.matches('/').count() as u32
    }

    pub fn parent(&self) -> Result<Self, PathError> {
        if self.is_root() {
            return Err(PathError::NoParent(self.0.clone()));
        }
        match self.0.rsplit_once('/') {
            Some(("", _)) => Ok(Self::root()),
            Some((parent, _)) => Ok(Self(parent.to_string())),
            None => Err(PathError::NoParent(self.0.clone())),
        }
    }

    pub fn child(&self, name: &str) -> Result<Self, PathError> {
        let name = validated(name)?;
        Ok(Self(match self.is_root() {
            true => format!("/{name}"),
            false => format!("{}/{name}", self.0),
        }))
    }

    /// Leading `/` is absolute; anything else names a child. Siblings need a
    /// full path — a relative guess would address an agent someone else owns.
    pub fn resolve(&self, reference: &str) -> Result<Self, PathError> {
        let reference = reference.trim();
        match reference.starts_with('/') {
            true => Self::parse(reference),
            false => reference
                .split('/')
                .try_fold(self.clone(), |path, segment| path.child(segment)),
        }
    }

    pub fn parse(text: &str) -> Result<Self, PathError> {
        let text = text.trim();
        if text == ROOT {
            return Ok(Self::root());
        }
        let Some(rest) = text.strip_prefix('/') else {
            return Err(PathError::Invalid(text.to_string()));
        };
        rest.split('/')
            .try_fold(Self::root(), |path, segment| path.child(segment))
    }

    pub fn under(&self, prefix: &Self) -> bool {
        prefix.is_root()
            || self == prefix
            || self
                .0
                .strip_prefix(prefix.as_str())
                .is_some_and(|rest| rest.starts_with('/'))
    }
}

fn validated(name: &str) -> Result<&str, PathError> {
    let name = name.trim();
    if name.is_empty() {
        return Err(PathError::Empty);
    }
    if !name
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
    {
        return Err(PathError::Invalid(name.to_string()));
    }
    Ok(name)
}

impl fmt::Display for AgentPath {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl serde::Serialize for AgentPath {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.0)
    }
}

impl<'de> serde::Deserialize<'de> for AgentPath {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let text = String::deserialize(deserializer)?;
        Self::parse(&text).map_err(serde::de::Error::custom)
    }
}

#[cfg(test)]
#[path = "path_tests.rs"]
mod tests;
