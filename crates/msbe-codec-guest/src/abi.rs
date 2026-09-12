//! Memory and host-call glue for the `msbe-pack-codec-1` ABI.
//!
//! Codec authors use [`crate::export_codec`] rather than this module. Every response is
//! `{"ok": value}` or `{"error": Error}`, returned as a pointer and length packed into one `i64`:
//! the pointer in the high 32 bits and the length in the low 32. Each call runs in a fresh
//! instance, so memory handed to the host is never freed and never reused.

use serde_json::{Map, Value};

use crate::Error;

/// Negative host input codes, mirrored from the host.
#[cfg(any(target_arch = "wasm32", test))]
mod codes {
    pub(super) const NOT_FOUND: i64 = -1;
    pub(super) const LIMIT: i64 = -2;
    pub(super) const UNSAFE_PATH: i64 = -3;
    pub(super) const BUDGET: i64 = -4;
}

/// Encodes a result as the ABI's JSON response.
pub fn encode(result: Result<Value, Error>) -> Vec<u8> {
    let (key, value) = match result {
        Ok(value) => ("ok", value),
        Err(error) => ("error", serde_json::to_value(&error).unwrap_or(Value::Null)),
    };
    let response = Value::Object(Map::from_iter([(key.to_owned(), value)]));
    serde_json::to_vec(&response).unwrap_or_else(|_| {
        br#"{"error":{"kind":"codec","message":"the response could not be encoded"}}"#.to_vec()
    })
}

/// Packs a guest address and length into the ABI's `i64` return value.
pub fn pack(address: usize, length: usize) -> i64 {
    let address = u64::try_from(address).unwrap_or(0) & 0xffff_ffff;
    let length = u64::try_from(length).unwrap_or(0) & 0xffff_ffff;
    i64::from_ne_bytes(((address << 32) | length).to_ne_bytes())
}

/// The codec error a negative host input code stands for.
#[cfg(any(target_arch = "wasm32", test))]
fn code_error(code: i64) -> Error {
    match code {
        codes::NOT_FOUND => Error::FormatMismatch,
        codes::LIMIT => Error::Limit {
            message: "the entry exceeds its read limit".to_owned(),
        },
        codes::UNSAFE_PATH => Error::UnsafePath {
            message: "the host refused an unsafe entry path".to_owned(),
        },
        codes::BUDGET => Error::Limit {
            message: "the call exceeded the host's read budget".to_owned(),
        },
        other => Error::codec(format!("host input failed with code {other}")),
    }
}

#[cfg(target_arch = "wasm32")]
pub use guest::{alloc, respond, with_request};
#[cfg(target_arch = "wasm32")]
pub(crate) use guest::{container, entries, read};

#[cfg(target_arch = "wasm32")]
#[expect(
    unsafe_code,
    reason = "the WebAssembly ABI crosses the host boundary through raw guest memory and imports"
)]
mod guest {
    use serde_json::Value;

    use super::{code_error, encode, pack};
    use crate::{Container, Error};

    #[link(wasm_import_module = "msbe_input")]
    unsafe extern "C" {
        #[link_name = "container"]
        fn host_container() -> i32;
        #[link_name = "entries"]
        fn host_entries() -> i32;
        #[link_name = "read"]
        fn host_read(path: *const u8, path_length: i32, limit: i64) -> i64;
        #[link_name = "take"]
        fn host_take(output: *mut u8, output_length: i32) -> i32;
    }

    /// Reserves `size` bytes the host writes a request into.
    pub fn alloc(size: i32) -> i32 {
        let length = usize::try_from(size).unwrap_or(0);
        let buffer: &'static mut [u8] = vec![0_u8; length].leak();
        i32::try_from(buffer.as_mut_ptr().expose_provenance()).unwrap_or(0)
    }

    /// Leaks an encoded response and returns its packed address and length.
    pub fn respond(result: Result<Value, Error>) -> i64 {
        let response: &'static [u8] = encode(result).leak();
        pack(response.as_ptr().expose_provenance(), response.len())
    }

    /// Decodes the request the host wrote at `pointer` and answers it with `answer`.
    pub fn with_request(
        pointer: i32,
        length: i32,
        answer: impl FnOnce(Value) -> Result<Value, Error>,
    ) -> i64 {
        let address = usize::try_from(pointer).unwrap_or(0);
        let length = usize::try_from(length).unwrap_or(0);
        // SAFETY: the host wrote `length` bytes at `address`, inside a buffer `alloc` leaked for it.
        let bytes = unsafe {
            std::slice::from_raw_parts(std::ptr::with_exposed_provenance::<u8>(address), length)
        };
        respond(
            serde_json::from_slice(bytes)
                .map_err(|error| Error::codec(format!("the host request is not JSON: {error}")))
                .and_then(answer),
        )
    }

    pub(crate) fn container() -> Container {
        // SAFETY: a host import without arguments.
        match unsafe { host_container() } {
            0 => Container::Zip,
            1 => Container::Directory,
            _ => Container::File,
        }
    }

    pub(crate) fn entries() -> Result<Vec<u8>, Error> {
        // SAFETY: a host import without arguments; it stages the listing for `take`.
        take(i64::from(unsafe { host_entries() }))
    }

    pub(crate) fn read(path: &str, limit: u64) -> Result<Vec<u8>, Error> {
        let path_length = i32::try_from(path.len()).map_err(|_| Error::UnsafePath {
            message: "the entry path is too long".to_owned(),
        })?;
        let limit = i64::try_from(limit).unwrap_or(i64::MAX);
        // SAFETY: `path` stays borrowed for the call, and the host reads exactly its bytes.
        take(unsafe { host_read(path.as_ptr(), path_length, limit) })
    }

    /// Copies the host's staged bytes, whose length the previous import returned.
    fn take(staged: i64) -> Result<Vec<u8>, Error> {
        if staged < 0 {
            return Err(code_error(staged));
        }
        let length = usize::try_from(staged).map_err(Error::codec)?;
        let mut buffer = vec![0_u8; length];
        let capacity = i32::try_from(length).map_err(Error::codec)?;
        // SAFETY: `buffer` is writable for `capacity` bytes for the duration of the call.
        let written = unsafe { host_take(buffer.as_mut_ptr(), capacity) };
        if written < 0 {
            return Err(code_error(i64::from(written)));
        }
        Ok(buffer)
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::{code_error, encode, pack};
    use crate::Error;

    #[test]
    fn responses_tag_results_and_typed_errors() {
        assert_eq!(
            encode(Ok(json!({"confidence": 0}))),
            br#"{"ok":{"confidence":0}}"#
        );
        assert_eq!(
            encode(Err(Error::UnsupportedTarget {
                game: "g".to_owned(),
                loader: "l".to_owned()
            })),
            br#"{"error":{"kind":"unsupported_target","game":"g","loader":"l"}}"#
        );
        assert_eq!(code_error(-1), Error::FormatMismatch);
    }

    #[test]
    fn packed_returns_keep_the_address_high_and_the_length_low() {
        let packed = u64::from_ne_bytes(pack(0x10, 0x20).to_ne_bytes());
        assert_eq!(packed >> 32, 0x10);
        assert_eq!(packed & 0xffff_ffff, 0x20);
    }
}
