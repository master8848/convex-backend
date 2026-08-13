use std::{
    ops::Deref,
    sync::LazyLock,
};

use regex::Regex;
use serde::{
    Deserialize,
    Serialize,
};
use value::ConvexString;

// Regex to restrict object keys to alphanumeric characters, /, -, _, and
// periods. This is more strict than S3's object naming requirements:
// https://docs.aws.amazon.com/AmazonS3/latest/userguide/object-keys.html
static OBJECT_KEY_REGEX: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^[a-zA-Z0-9-_./]+$").unwrap());

#[derive(Clone, Debug, Eq, PartialEq, PartialOrd, Ord, Hash)]
#[must_use]
pub struct ObjectKey(String);

/// Fully qualified object key. For s3, in the format
/// {bucket}/{prefix}-{object_key}
#[derive(
    Debug,
    Clone,
    Eq,
    PartialEq,
    Ord,
    PartialOrd,
    Serialize,
    Deserialize,
    derive_more::From,
    derive_more::Into,
)]
pub struct FullyQualifiedObjectKey(String);

impl FullyQualifiedObjectKey {
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl TryFrom<ObjectKey> for ConvexString {
    type Error = anyhow::Error;

    fn try_from(value: ObjectKey) -> Result<Self, Self::Error> {
        value.to_string().try_into()
    }
}

impl TryFrom<ConvexString> for ObjectKey {
    type Error = anyhow::Error;

    fn try_from(value: ConvexString) -> Result<Self, Self::Error> {
        String::from(value).try_into()
    }
}

impl TryFrom<String> for ObjectKey {
    type Error = anyhow::Error;

    fn try_from(s: String) -> anyhow::Result<Self> {
        anyhow::ensure!(OBJECT_KEY_REGEX.is_match(&s));
        // Disallow path traversal: no component may be empty, ".", or contain
        // "..", otherwise joining a key onto a local storage directory could
        // escape it (e.g. via a leading "/" or "../").
        anyhow::ensure!(s.split('/').all(|component| {
            !component.is_empty() && component != "." && !component.contains("..")
        }));
        Ok(Self(s))
    }
}

impl TryFrom<&str> for ObjectKey {
    type Error = anyhow::Error;

    fn try_from(s: &str) -> anyhow::Result<Self> {
        s.to_string().try_into()
    }
}

impl From<ObjectKey> for String {
    fn from(key: ObjectKey) -> String {
        key.0
    }
}

impl Deref for ObjectKey {
    type Target = str;

    fn deref(&self) -> &str {
        &self.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn object_key_rejects_path_traversal() {
        for s in [
            "..",
            "../foo",
            "foo/../bar",
            "foo/..",
            "foo/..bar",
            "foo/bar..",
            "/etc/passwd",
            "//etc/passwd",
            "foo//bar",
            "foo/",
            "/foo",
        ] {
            assert!(
                ObjectKey::try_from(s.to_string()).is_err(),
                "expected {s} to be rejected"
            );
        }
    }

    #[test]
    fn object_key_accepts_valid_keys() {
        for s in ["abc", "a-b_c.d", "foo/bar", "0f9a5d6e-a9b0-4d2f", "0"] {
            assert!(
                ObjectKey::try_from(s.to_string()).is_ok(),
                "expected {s} to be accepted"
            );
        }
    }
}
