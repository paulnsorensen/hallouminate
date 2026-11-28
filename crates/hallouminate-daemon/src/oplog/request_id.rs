use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Serialize};

const REQUEST_ID_LEN: usize = 32;

/// Correlation id that joins the client row and the daemon row of one call.
///
/// The value is 32 lowercase hexadecimal characters, the W3C trace-id shape.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct RequestId(String);

impl RequestId {
    /// Generates a random request id.
    pub fn generate() -> Self {
        Self(format!(
            "{:016x}{:016x}",
            fastrand::u64(..),
            fastrand::u64(..)
        ))
    }

    /// Returns the id as a string slice.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for RequestId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// Error returned when a string is not a valid [`RequestId`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InvalidRequestId;

impl fmt::Display for InvalidRequestId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("request_id must be 32 lowercase hexadecimal characters")
    }
}

impl std::error::Error for InvalidRequestId {}

fn is_valid(value: &str) -> bool {
    if value.len() != REQUEST_ID_LEN {
        return false;
    }
    for byte in value.bytes() {
        if !(byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte)) {
            return false;
        }
    }
    true
}

impl TryFrom<String> for RequestId {
    type Error = InvalidRequestId;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        if !is_valid(&value) {
            return Err(InvalidRequestId);
        }
        Ok(Self(value))
    }
}

impl FromStr for RequestId {
    type Err = InvalidRequestId;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        Self::try_from(value.to_owned())
    }
}

impl From<RequestId> for String {
    fn from(value: RequestId) -> Self {
        value.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generated_ids_parse_and_differ() {
        let first = RequestId::generate();
        let second = RequestId::generate();
        assert_ne!(first, second);
        for id in [first, second] {
            assert_eq!(id.as_str().len(), 32);
            let parsed: RequestId = id.as_str().parse().expect("generated id parses");
            assert_eq!(parsed, id);
        }
    }

    #[test]
    fn parse_rejects_wrong_shape() {
        for value in [
            "",
            "0123456789abcdef0123456789abcde",
            "0123456789abcdef0123456789abcdef0",
            "0123456789ABCDEF0123456789abcdef",
            "0123456789abcdef0123456789abcdeg",
            "0123456789abcdef-123456789abcdef",
        ] {
            assert_eq!(
                value.parse::<RequestId>(),
                Err(InvalidRequestId),
                "{value:?}"
            );
        }
    }

    #[test]
    fn serde_uses_validated_string() {
        let id: RequestId = "0123456789abcdef0123456789abcdef"
            .parse()
            .expect("valid id");
        let json = serde_json::to_string(&id).expect("serialize");
        assert_eq!(json, "\"0123456789abcdef0123456789abcdef\"");
        let back: RequestId = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(back, id);
        assert!(serde_json::from_str::<RequestId>("\"XYZ\"").is_err());
    }
}
