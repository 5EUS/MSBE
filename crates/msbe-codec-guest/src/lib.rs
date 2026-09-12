//! The guest side of MSBE's sandboxed pack codec ABI, `msbe-pack-codec-1`.
//!
//! A codec built with this crate compiles to `wasm32-unknown-unknown`. It exports the functions the
//! MSBE host calls, and reads its pack only through the host's bounded input imports. It has no
//! filesystem, network, clock or randomness: those imports do not exist in its world, and the host
//! refuses a module that asks for them. Records cross the boundary as JSON in the shapes
//! `msbe-provider-api` defines, so a codec may work with [`Value`]s or with its own serde types.
//!
//! ```ignore
//! struct MyFormat;
//!
//! impl msbe_codec_guest::Codec for MyFormat {
//!     // descriptor, probe, plan_import, plan_export, layout
//! }
//!
//! msbe_codec_guest::export_codec!(MyFormat);
//! ```
//!
//! Off `wasm32`, [`Input::memory`] serves a pack from memory, so a codec's logic can be unit
//! tested natively. See `docs/18-wasm-codecs.md` for the ABI, the record shapes, and packaging.

#[doc(hidden)]
pub mod abi;
mod error;
mod input;

pub use error::Error;
pub use input::{Container, Entry, Input};
pub use serde_json::{self, Value, json};

/// The codec ABI version this crate implements.
pub const ABI_VERSION: i32 = 1;

/// A pack format implementation served from a sandbox.
///
/// Every method is a pure translation: the same inputs must produce the same output. The host
/// validates every returned record against its typed contract before using it.
pub trait Codec {
    /// The codec descriptor, in the host's `PackCodecDescriptor` shape.
    fn descriptor() -> Value;

    /// Bounded format detection, returning `{"confidence": 0..=100, "reason": string | null}`.
    ///
    /// # Errors
    ///
    /// Returns an [`Error`] only when the input cannot be inspected; a pack in another format is
    /// confidence zero, not an error.
    fn probe(input: &Input) -> Result<Value, Error>;

    /// Plans an import. `request` is `{"context": PackImportContext, "options": PackOptions}`, and
    /// the result is a `PackImportPlan`.
    ///
    /// # Errors
    ///
    /// Returns an [`Error`] when the pack is malformed or cannot be represented.
    fn plan_import(input: &Input, request: Value) -> Result<Value, Error>;

    /// Plans an export. `request` is `{"context": {game, target, lockfile, files, observations,
    /// inclusion}, "options": PackOptions}`, and the result is a `PackExportPlan`.
    ///
    /// # Errors
    ///
    /// Returns an [`Error`] when the profile cannot be represented in this format.
    fn plan_export(request: Value) -> Result<Value, Error>;

    /// Maps a `PackExportPlan` this codec produced to a `PackLayout`.
    ///
    /// # Errors
    ///
    /// Returns an [`Error`] when the plan is not one this codec produced.
    fn layout(plan: Value) -> Result<Value, Error>;
}

/// Exports `$codec`, which implements [`Codec`], through the `msbe-pack-codec-1` ABI.
///
/// The exports exist only when compiling for `wasm32`, so a codec crate also builds and tests
/// natively.
#[macro_export]
macro_rules! export_codec {
    ($codec:ty) => {
        #[cfg(target_arch = "wasm32")]
        const _: () = {
            #[unsafe(no_mangle)]
            extern "C" fn msbe_abi_version() -> i32 {
                $crate::ABI_VERSION
            }

            #[unsafe(no_mangle)]
            extern "C" fn msbe_alloc(size: i32) -> i32 {
                $crate::abi::alloc(size)
            }

            #[unsafe(no_mangle)]
            extern "C" fn msbe_descriptor() -> i64 {
                $crate::abi::respond(Ok(<$codec as $crate::Codec>::descriptor()))
            }

            #[unsafe(no_mangle)]
            extern "C" fn msbe_probe() -> i64 {
                $crate::abi::respond(<$codec as $crate::Codec>::probe(&$crate::Input::host()))
            }

            #[unsafe(no_mangle)]
            extern "C" fn msbe_plan_import(pointer: i32, length: i32) -> i64 {
                $crate::abi::with_request(pointer, length, |request| {
                    <$codec as $crate::Codec>::plan_import(&$crate::Input::host(), request)
                })
            }

            #[unsafe(no_mangle)]
            extern "C" fn msbe_plan_export(pointer: i32, length: i32) -> i64 {
                $crate::abi::with_request(pointer, length, <$codec as $crate::Codec>::plan_export)
            }

            #[unsafe(no_mangle)]
            extern "C" fn msbe_layout(pointer: i32, length: i32) -> i64 {
                $crate::abi::with_request(pointer, length, <$codec as $crate::Codec>::layout)
            }
        };
    };
}
