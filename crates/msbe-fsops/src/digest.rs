//! SHA-256 content digests.

use std::{
    fmt,
    io::{self, Read},
    str::FromStr,
};

use serde::{Deserialize, Deserializer, Serialize, Serializer};
use sha2::{Digest as _, Sha256};

use crate::error::Error;

/// A SHA-256 digest that names a blob by its contents.
///
/// Displayed and serialized as `sha256:` followed by 64 lowercase hex digits, which is the
/// only form [`FromStr`] accepts.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Digest([u8; 32]);

impl Digest {
    /// Hashes a byte slice.
    pub fn of_bytes(bytes: &[u8]) -> Self {
        let mut hasher = StreamHasher::new();
        hasher.update(bytes);
        hasher.finish()
    }

    /// Hashes everything `reader` yields.
    ///
    /// # Errors
    ///
    /// Returns the underlying error if a read fails.
    pub fn of_reader(mut reader: impl Read) -> io::Result<Self> {
        let mut hasher = StreamHasher::new();
        let mut buf = vec![0_u8; 64 * 1024];
        loop {
            let read = reader.read(&mut buf)?;
            if read == 0 {
                return Ok(hasher.finish());
            }
            hasher.update(buf.get(..read).unwrap_or_default());
        }
    }

    /// The first byte and the remaining 31 as hex, for directory fan-out.
    pub(crate) fn fanout(&self) -> (String, String) {
        let [first, rest @ ..] = self.0;
        (hex(&[first]), hex(&rest))
    }
}

fn hex(bytes: &[u8]) -> String {
    use fmt::Write as _;
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        // Formatting into a String cannot fail.
        let _ = write!(out, "{byte:02x}");
    }
    out
}

const fn nibble(c: u8) -> Option<u8> {
    match c {
        b'0'..=b'9' => Some(c - b'0'),
        b'a'..=b'f' => Some(c - b'a' + 10),
        _ => None,
    }
}

/// Incremental SHA-256 for callers that stream data elsewhere at the same time.
pub(crate) struct StreamHasher(Sha256);

impl StreamHasher {
    pub(crate) fn new() -> Self {
        Self(Sha256::new())
    }

    pub(crate) fn update(&mut self, bytes: &[u8]) {
        self.0.update(bytes);
    }

    pub(crate) fn finish(self) -> Digest {
        let output = self.0.finalize();
        let mut bytes = [0_u8; 32];
        bytes.copy_from_slice(&output);
        Digest(bytes)
    }
}

impl fmt::Display for Digest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "sha256:{}", hex(&self.0))
    }
}

impl fmt::Debug for Digest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(self, f)
    }
}

impl FromStr for Digest {
    type Err = Error;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let invalid = || Error::InvalidDigest(s.to_owned());
        let digits = s.strip_prefix("sha256:").ok_or_else(invalid)?;
        if digits.len() != 64 {
            return Err(invalid());
        }
        let mut bytes = [0_u8; 32];
        for (slot, pair) in bytes.iter_mut().zip(digits.as_bytes().chunks_exact(2)) {
            let [hi, lo] = pair else {
                return Err(invalid());
            };
            let hi = nibble(*hi).ok_or_else(invalid)?;
            let lo = nibble(*lo).ok_or_else(invalid)?;
            *slot = (hi << 4) | lo;
        }
        Ok(Self(bytes))
    }
}

impl Serialize for Digest {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_str(self)
    }
}

impl<'de> Deserialize<'de> for Digest {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let text = String::deserialize(deserializer)?;
        text.parse().map_err(serde::de::Error::custom)
    }
}

#[cfg(test)]
mod tests {
    use super::Digest;

    const ABC: &str = "sha256:ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad";

    #[test]
    fn matches_the_published_test_vector() {
        assert_eq!(Digest::of_bytes(b"abc").to_string(), ABC);
    }

    #[test]
    fn streaming_matches_one_shot_hashing() {
        let data: Vec<u8> = (0..200_000_u32)
            .map(|i| u8::try_from(i % 251).unwrap())
            .collect();
        assert_eq!(
            Digest::of_reader(data.as_slice()).unwrap(),
            Digest::of_bytes(&data)
        );
    }

    #[test]
    fn parses_only_the_canonical_form() {
        let digest: Digest = ABC.parse().unwrap();
        assert_eq!(digest.to_string(), ABC);
        for bad in [
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad",
            "sha256:BA7816BF8F01CFEA414140DE5DAE2223B00361A396177A9CB410FF61F20015AD",
            "sha256:ba78",
            "sha256:zz7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad",
            "md5:ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad",
        ] {
            assert!(bad.parse::<Digest>().is_err(), "{bad} should be rejected");
        }
    }

    #[test]
    fn serializes_as_a_string() {
        let digest = Digest::of_bytes(b"abc");
        let json = serde_json::to_string(&digest).unwrap();
        assert_eq!(json, format!("\"{ABC}\""));
        assert_eq!(serde_json::from_str::<Digest>(&json).unwrap(), digest);
    }

    #[test]
    fn fans_out_by_first_byte() {
        let (head, tail) = Digest::of_bytes(b"abc").fanout();
        assert_eq!(head, "ba");
        assert_eq!(tail.len(), 62);
    }
}
