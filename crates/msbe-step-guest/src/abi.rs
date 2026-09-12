//! Memory and host-call glue for the `msbe-plan-step-1` ABI.
//!
//! Step authors use [`crate::export_step`] rather than this module. The host writes a JSON request
//! into memory `msbe_alloc` reserved, calls `msbe_run`, and reads back `{"ok": {"operations":
//! [...]}}` or `{"error": Error}`, returned as a pointer and length packed into one `i64`: the
//! pointer in the high 32 bits and the length in the low 32. Each call runs in a fresh instance, so
//! memory handed to the host is never freed and never reused.

use serde::Serialize;

use crate::{Error, Operation};

/// Negative host return codes, mirrored from the host.
#[cfg(any(target_arch = "wasm32", test))]
mod codes {
    pub(super) const NOT_FOUND: i64 = -1;
    pub(super) const LIMIT: i64 = -2;
    pub(super) const UNSAFE_PATH: i64 = -3;
    pub(super) const BUDGET: i64 = -4;
    pub(super) const QUESTION_PENDING: i64 = -5;
    pub(super) const DENIED: i64 = -6;
}

#[derive(Serialize)]
enum Response {
    #[serde(rename = "ok")]
    Ok { operations: Vec<Operation> },
    #[serde(rename = "error")]
    Error(Error),
}

/// Encodes a step result as the ABI's JSON response.
pub fn encode(result: Result<Vec<Operation>, Error>) -> Vec<u8> {
    let response = match result {
        Ok(operations) => Response::Ok { operations },
        Err(error) => Response::Error(error),
    };
    serde_json::to_vec(&response).unwrap_or_else(|_| {
        br#"{"error":{"kind":"extension","message":"the response could not be encoded"}}"#.to_vec()
    })
}

/// Packs a guest address and length into the ABI's `i64` return value.
pub fn pack(address: usize, length: usize) -> i64 {
    let address = u64::try_from(address).unwrap_or(0) & 0xffff_ffff;
    let length = u64::try_from(length).unwrap_or(0) & 0xffff_ffff;
    i64::from_ne_bytes(((address << 32) | length).to_ne_bytes())
}

/// The step error a negative host return code stands for, for the call about `subject`.
#[cfg(any(target_arch = "wasm32", test))]
fn code_error(code: i64, subject: &str) -> Error {
    match code {
        codes::NOT_FOUND => Error::invalid_archive(format!("{subject} was not found")),
        codes::LIMIT => Error::Limit {
            message: format!("{subject} exceeds its read limit"),
        },
        codes::UNSAFE_PATH => Error::UnsafePath {
            message: format!("{subject} is not a safe relative path"),
        },
        codes::BUDGET => Error::Limit {
            message: format!("reading {subject} exceeds the host's read budget"),
        },
        codes::QUESTION_PENDING => Error::QuestionPending {
            question: subject.to_owned(),
        },
        codes::DENIED => Error::Denied {
            message: format!("the step is not granted {subject}"),
        },
        other => Error::extension(format!("the host refused {subject} with code {other}")),
    }
}

#[cfg(target_arch = "wasm32")]
pub use guest::{alloc, run};
#[cfg(target_arch = "wasm32")]
pub(crate) use guest::{ask, entries, log, read, read_game};

#[cfg(target_arch = "wasm32")]
#[expect(
    unsafe_code,
    reason = "the WebAssembly ABI crosses the host boundary through raw guest memory and imports"
)]
mod guest {
    use super::{code_error, codes, encode, pack};
    use crate::{Context, Error, Operation, Request};

    #[link(wasm_import_module = "msbe_host")]
    unsafe extern "C" {
        #[link_name = "take"]
        fn host_take(output: *mut u8, output_length: i32) -> i32;
        #[link_name = "log"]
        fn host_log(message: *const u8, message_length: i32);
    }

    #[link(wasm_import_module = "msbe_archive")]
    unsafe extern "C" {
        #[link_name = "entries"]
        fn archive_entries() -> i64;
        #[link_name = "read"]
        fn archive_read(path: *const u8, path_length: i32, limit: i64) -> i64;
    }

    #[link(wasm_import_module = "msbe_game")]
    unsafe extern "C" {
        #[link_name = "read"]
        fn game_read(path: *const u8, path_length: i32, limit: i64) -> i64;
    }

    #[link(wasm_import_module = "msbe_ui")]
    unsafe extern "C" {
        #[link_name = "ask"]
        fn ui_ask(question: *const u8, question_length: i32) -> i64;
    }

    /// Reserves `size` bytes the host writes a request into.
    pub fn alloc(size: i32) -> i32 {
        let length = usize::try_from(size).unwrap_or(0);
        let buffer: &'static mut [u8] = vec![0_u8; length].leak();
        i32::try_from(buffer.as_mut_ptr().expose_provenance()).unwrap_or(0)
    }

    /// Decodes the request the host wrote at `pointer`, runs `step` over it, and returns the
    /// packed response.
    pub fn run(
        pointer: i32,
        length: i32,
        step: impl FnOnce(&Context, Request) -> Result<Vec<Operation>, Error>,
    ) -> i64 {
        let address = usize::try_from(pointer).unwrap_or(0);
        let length = usize::try_from(length).unwrap_or(0);
        // SAFETY: the host wrote `length` bytes at `address`, inside a buffer `alloc` leaked for it.
        let bytes = unsafe {
            std::slice::from_raw_parts(std::ptr::with_exposed_provenance::<u8>(address), length)
        };
        let result = serde_json::from_slice::<Request>(bytes)
            .map_err(|error| Error::extension(format!("the host request is not valid: {error}")))
            .and_then(|request| step(&Context::host(), request));
        let response: &'static [u8] = encode(result).leak();
        pack(response.as_ptr().expose_provenance(), response.len())
    }

    pub(crate) fn entries() -> Result<Vec<u8>, Error> {
        // SAFETY: a host import without arguments; it stages the listing for `take`.
        take(unsafe { archive_entries() }, "the mod's file list")
    }

    pub(crate) fn read(path: &str, limit: u64) -> Result<Vec<u8>, Error> {
        let (length, limit) = arguments(path, limit)?;
        // SAFETY: `path` stays borrowed for the call, and the host reads exactly its bytes.
        take(unsafe { archive_read(path.as_ptr(), length, limit) }, path)
    }

    pub(crate) fn read_game(path: &str, limit: u64) -> Result<Option<Vec<u8>>, Error> {
        let (length, limit) = arguments(path, limit)?;
        // SAFETY: `path` stays borrowed for the call, and the host reads exactly its bytes.
        match unsafe { game_read(path.as_ptr(), length, limit) } {
            codes::NOT_FOUND => Ok(None),
            staged => take(staged, path).map(Some),
        }
    }

    pub(crate) fn ask(question: &[u8], id: &str) -> Result<String, Error> {
        let length = i32::try_from(question.len()).map_err(|_| Error::Limit {
            message: format!("question {id} is too large"),
        })?;
        // SAFETY: `question` stays borrowed for the call, and the host reads exactly its bytes.
        let answer = take(unsafe { ui_ask(question.as_ptr(), length) }, id)?;
        String::from_utf8(answer).map_err(Error::extension)
    }

    pub(crate) fn log(message: &str) {
        let length = i32::try_from(message.len()).unwrap_or(i32::MAX);
        // SAFETY: `message` stays borrowed for the call, and the host reads at most its bytes.
        unsafe { host_log(message.as_ptr(), length) }
    }

    fn arguments(path: &str, limit: u64) -> Result<(i32, i64), Error> {
        let length = i32::try_from(path.len()).map_err(|_| Error::UnsafePath {
            message: "the path is too long".to_owned(),
        })?;
        Ok((length, i64::try_from(limit).unwrap_or(i64::MAX)))
    }

    /// Copies the host's staged bytes, whose length the previous import returned.
    fn take(staged: i64, subject: &str) -> Result<Vec<u8>, Error> {
        if staged < 0 {
            return Err(code_error(staged, subject));
        }
        let length = usize::try_from(staged).map_err(Error::extension)?;
        let mut buffer = vec![0_u8; length];
        let capacity = i32::try_from(length).map_err(Error::extension)?;
        // SAFETY: `buffer` is writable for `capacity` bytes for the duration of the call.
        let written = unsafe { host_take(buffer.as_mut_ptr(), capacity) };
        if written < 0 {
            return Err(code_error(i64::from(written), subject));
        }
        Ok(buffer)
    }
}

#[cfg(test)]
mod tests {
    use serde_json::{Value, json};

    use super::{code_error, encode, pack};
    use crate::{Error, Operation};

    #[test]
    fn responses_carry_operations_or_typed_errors() {
        let decode = |bytes: Vec<u8>| serde_json::from_slice::<Value>(&bytes).ok();
        let placed = encode(Ok(vec![Operation::Place {
            source: "a.txt".to_owned(),
            path: "mods/a.txt".to_owned(),
        }]));
        assert_eq!(
            decode(placed),
            Some(json!({"ok": {"operations": [
                {"kind": "place", "source": "a.txt", "path": "mods/a.txt"}
            ]}}))
        );
        let pending = encode(Err(Error::QuestionPending {
            question: "size".to_owned(),
        }));
        assert_eq!(
            decode(pending),
            Some(json!({"error": {"kind": "question-pending", "question": "size"}}))
        );
        assert_eq!(
            code_error(-5, "size"),
            Error::QuestionPending {
                question: "size".to_owned()
            }
        );
    }

    #[test]
    fn packed_returns_keep_the_address_high_and_the_length_low() {
        let packed = u64::from_ne_bytes(pack(0x10, 0x20).to_ne_bytes());
        assert_eq!(packed >> 32, 0x10);
        assert_eq!(packed & 0xffff_ffff, 0x20);
    }
}
