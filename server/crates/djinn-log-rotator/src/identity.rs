use std::fmt;
use std::str::FromStr;

use thiserror::Error;
use uuid::Uuid;

#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum IdentityError {
    #[error("invalid {kind}: {value:?}")]
    Invalid { kind: &'static str, value: String },
}

fn dns_label(value: &str, kind: &'static str) -> Result<(), IdentityError> {
    let valid = !value.is_empty()
        && value.len() <= 63
        && value
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
        && !value.starts_with('-')
        && !value.ends_with('-');
    valid.then_some(()).ok_or_else(|| IdentityError::Invalid {
        kind,
        value: value.to_owned(),
    })
}

macro_rules! dns_identity {
    ($name:ident, $kind:literal) => {
        #[derive(Debug, Clone, PartialEq, Eq, Hash)]
        pub struct $name(String);
        impl $name {
            pub fn new(value: impl Into<String>) -> Result<Self, IdentityError> {
                let value = value.into();
                dns_label(&value, $kind)?;
                Ok(Self(value))
            }
            pub fn as_str(&self) -> &str {
                &self.0
            }
        }
        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                self.0.fmt(f)
            }
        }
        impl FromStr for $name {
            type Err = IdentityError;
            fn from_str(value: &str) -> Result<Self, Self::Err> {
                Self::new(value)
            }
        }
    };
}

dns_identity!(Namespace, "namespace");

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ContainerName(String);
impl ContainerName {
    pub fn new(value: impl Into<String>) -> Result<Self, IdentityError> {
        let value = value.into();
        let valid = !value.is_empty()
            && value.len() <= 128
            && value
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'))
            && !value.starts_with(['.', '_', '-'])
            && !value.ends_with(['.', '_', '-']);
        if valid {
            Ok(Self(value))
        } else {
            Err(IdentityError::Invalid {
                kind: "container",
                value,
            })
        }
    }
    pub fn as_str(&self) -> &str {
        &self.0
    }
}
impl fmt::Display for ContainerName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}
impl FromStr for ContainerName {
    type Err = IdentityError;
    fn from_str(value: &str) -> Result<Self, Self::Err> {
        Self::new(value)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct PodUid(Uuid);
impl PodUid {
    pub fn new(value: impl AsRef<str>) -> Result<Self, IdentityError> {
        let value = value.as_ref();
        Uuid::parse_str(value)
            .map(Self)
            .map_err(|_| IdentityError::Invalid {
                kind: "pod UID",
                value: value.to_owned(),
            })
    }
    pub fn as_str(&self) -> String {
        self.0.hyphenated().to_string()
    }
}
impl fmt::Display for PodUid {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.hyphenated().fmt(f)
    }
}
impl FromStr for PodUid {
    type Err = IdentityError;
    fn from_str(value: &str) -> Result<Self, Self::Err> {
        Self::new(value)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct StreamIdentity {
    pub namespace: Namespace,
    pub pod_uid: PodUid,
    pub container: ContainerName,
}
impl StreamIdentity {
    pub fn new(namespace: Namespace, pod_uid: PodUid, container: ContainerName) -> Self {
        Self {
            namespace,
            pod_uid,
            container,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn rejects_path_components() {
        assert!(Namespace::new("../system").is_err());
        assert!(ContainerName::new("..").is_err());
        assert!(PodUid::new("../../etc").is_err());
    }
}
