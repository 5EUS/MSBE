//! The redaction filter in front of everything the daemon writes
//! (`docs/07-browser-and-secrets.md` §7.5).
//!
//! It works on values rather than call sites: every [`Secret`](crate::Secret) created in this
//! process is registered here, and [`redact`] replaces each registered value wherever it appears.
//! A secret never loaded into the process cannot appear in its output either.

use std::{
    borrow::Cow,
    cmp::Reverse,
    fmt,
    io::{self, Write},
    mem,
    sync::{Mutex, PoisonError},
};

use zeroize::Zeroizing;

/// What a redacted secret is replaced with.
pub const REDACTED: &str = "<redacted>";

/// Every secret value this process has held, longest first.
static SECRETS: Mutex<Vec<Zeroizing<String>>> = Mutex::new(Vec::new());

/// Registers `value` for redaction for the rest of the process.
pub(crate) fn register(value: &str) {
    let mut secrets = SECRETS.lock().unwrap_or_else(PoisonError::into_inner);
    if secrets.iter().any(|known| known.as_str() == value) {
        return;
    }
    secrets.push(Zeroizing::new(value.to_owned()));
    // Longest first, so a secret that contains another is replaced whole.
    secrets.sort_by_key(|known| Reverse(known.len()));
}

/// `text` with every secret this process has held replaced by [`REDACTED`].
pub fn redact(text: &str) -> Cow<'_, str> {
    let secrets = SECRETS.lock().unwrap_or_else(PoisonError::into_inner);
    if !secrets.iter().any(|secret| text.contains(secret.as_str())) {
        return Cow::Borrowed(text);
    }
    let mut current = Zeroizing::new(text.to_owned());
    // A replacement could, in principle, complete another secret across its edge, so passes repeat
    // until nothing matches. Text that still holds a secret after one pass more than there are
    // secrets is withheld entirely.
    for _ in 0..=secrets.len() {
        let mut changed = false;
        for secret in secrets.iter() {
            if current.contains(secret.as_str()) {
                current = Zeroizing::new(current.replace(secret.as_str(), REDACTED));
                changed = true;
            }
        }
        if !changed {
            return Cow::Owned(mem::take(&mut *current));
        }
    }
    Cow::Borrowed(REDACTED)
}

/// A writer that redacts each line before passing it on, for output that is not a daemon response.
///
/// A secret never contains a line break, so none is split between two lines. Output is held until
/// its line ends, or the writer is flushed or dropped.
pub struct RedactingWriter<W: Write> {
    inner: W,
    pending: Zeroizing<Vec<u8>>,
}

impl<W: Write> RedactingWriter<W> {
    /// Redacts what is written before writing it to `inner`.
    pub fn new(inner: W) -> Self {
        Self {
            inner,
            pending: Zeroizing::new(Vec::new()),
        }
    }

    fn emit(&mut self, line: &[u8]) -> io::Result<()> {
        let text = String::from_utf8_lossy(line);
        self.inner.write_all(redact(&text).as_bytes())
    }
}

impl<W: Write> Write for RedactingWriter<W> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.pending.extend_from_slice(buf);
        while let Some(end) = self.pending.iter().position(|byte| *byte == b'\n') {
            let line = Zeroizing::new(self.pending.drain(..=end).collect::<Vec<u8>>());
            self.emit(&line)?;
        }
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        if !self.pending.is_empty() {
            let rest = Zeroizing::new(mem::take(&mut *self.pending));
            self.emit(&rest)?;
        }
        self.inner.flush()
    }
}

impl<W: Write> Drop for RedactingWriter<W> {
    fn drop(&mut self) {
        // Nothing is left to report a failed final write to.
        drop(self.flush());
    }
}

impl<W: Write> fmt::Debug for RedactingWriter<W> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("RedactingWriter")
            .field("pending_bytes", &self.pending.len())
            .finish_non_exhaustive()
    }
}

#[cfg(test)]
mod tests {
    use std::io::Write as _;

    use super::{REDACTED, RedactingWriter, redact};
    use crate::Secret;

    /// A small deterministic generator, so a failing case reproduces.
    struct XorShift(u64);

    impl XorShift {
        fn next(&mut self) -> u64 {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            self.0
        }

        fn below(&mut self, bound: usize) -> usize {
            usize::try_from(self.next() % u64::try_from(bound).unwrap()).unwrap()
        }

        fn text(&mut self, alphabet: &[u8], length: usize) -> String {
            (0..length)
                .map(|_| char::from(*alphabet.get(self.below(alphabet.len())).unwrap()))
                .collect()
        }
    }

    #[test]
    fn no_registered_secret_survives_redaction() {
        const SECRET_ALPHABET: &[u8] =
            b"abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789-_.";
        const TEXT_ALPHABET: &[u8] = b"abcdefXYZ0189 -_:/?=&\"{}[]\n";
        let mut random = XorShift(0x9E37_79B9_7F4A_7C15);
        for _ in 0..300 {
            let length = 8 + random.below(48);
            let secret = Secret::new(random.text(SECRET_ALPHABET, length)).unwrap();
            let value = secret.expose();
            let mut message = String::new();
            for _ in 0..random.below(6) {
                let filler = random.below(12);
                message.push_str(&random.text(TEXT_ALPHABET, filler));
                match random.below(4) {
                    0 => message.push_str(value),
                    1 => {
                        message.push_str(value);
                        message.push_str(value);
                    }
                    2 => message.push_str(value.get(..value.len() - 1).unwrap()),
                    _ => message.push_str(value.get(1..).unwrap()),
                }
            }
            let redacted = redact(&message);
            assert!(
                !redacted.contains(value),
                "{value:?} survived in {redacted:?}"
            );
            if !message.contains(value) {
                assert_eq!(redacted, message);
            }
        }
    }

    #[test]
    fn a_secret_inside_a_longer_one_leaves_nothing_of_either() {
        let inner = Secret::new("inner-token-4c1d".to_owned()).unwrap();
        let outer = Secret::new(format!("prefix-{}-suffix", inner.expose())).unwrap();
        let message = format!("sent {} and {}", outer.expose(), inner.expose());
        assert_eq!(redact(&message), format!("sent {REDACTED} and {REDACTED}"));
    }

    #[test]
    fn the_writer_redacts_a_secret_written_in_pieces() {
        let secret = Secret::new("split-across-writes-9f2e".to_owned()).unwrap();
        let mut output = Vec::new();
        {
            let mut writer = RedactingWriter::new(&mut output);
            let (left, right) = secret.expose().split_at(7);
            write!(writer, "error: {left}").unwrap();
            write!(writer, "{right} refused\nunfinished {}", secret.expose()).unwrap();
        }
        assert_eq!(
            String::from_utf8(output).unwrap(),
            format!("error: {REDACTED} refused\nunfinished {REDACTED}")
        );
    }
}
