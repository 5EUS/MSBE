//! Hashing downloads as they stream to disk.

use std::{
    fmt::Write as _,
    io::{self, Write},
};

use md5::Md5;
use sha1::Sha1;
use sha2::{Digest as _, Sha256, Sha512};

/// Hashes everything written through it with every digest a provider may publish, and counts the
/// bytes.
///
/// SHA-256 and SHA-512 identify content. SHA-1 and MD5 are computed only so a download can be
/// checked against the weaker digest a catalog publishes; they catch a wrong or corrupted file,
/// not a deliberately colliding one.
pub(crate) struct HashingWriter<W> {
    inner: W,
    md5: Md5,
    sha1: Sha1,
    sha256: Sha256,
    sha512: Sha512,
    written: u64,
}

impl<W> HashingWriter<W> {
    pub(crate) fn new(inner: W) -> Self {
        Self {
            inner,
            md5: Md5::new(),
            sha1: Sha1::new(),
            sha256: Sha256::new(),
            sha512: Sha512::new(),
            written: 0,
        }
    }

    /// The writer being hashed into.
    pub(crate) const fn inner(&self) -> &W {
        &self.inner
    }

    /// The bytes written so far.
    pub(crate) const fn written(&self) -> u64 {
        self.written
    }

    /// The MD5 of everything written so far, as lowercase hex.
    pub(crate) fn md5_hex(&self) -> String {
        hex(&self.md5.clone().finalize())
    }

    /// The SHA-1 of everything written so far, as lowercase hex.
    pub(crate) fn sha1_hex(&self) -> String {
        hex(&self.sha1.clone().finalize())
    }

    /// The SHA-256 of everything written so far, as lowercase hex.
    pub(crate) fn sha256_hex(&self) -> String {
        hex(&self.sha256.clone().finalize())
    }

    /// The SHA-512 of everything written so far, as lowercase hex.
    pub(crate) fn sha512_hex(&self) -> String {
        hex(&self.sha512.clone().finalize())
    }
}

impl<W: Write> Write for HashingWriter<W> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let accepted = self.inner.write(buf)?;
        let chunk = buf.get(..accepted).unwrap_or_default();
        self.md5.update(chunk);
        self.sha1.update(chunk);
        self.sha256.update(chunk);
        self.sha512.update(chunk);
        self.written += accepted as u64;
        Ok(accepted)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.inner.flush()
    }
}

/// Lowercase hex, the form providers publish digests in.
pub fn hex(bytes: &[u8]) -> String {
    bytes
        .iter()
        .fold(String::with_capacity(bytes.len() * 2), |mut out, byte| {
            // Formatting into a String cannot fail.
            let _ = write!(out, "{byte:02x}");
            out
        })
}
