//! The guest side of MSBE's sandboxed plan step ABI, `msbe-plan-step-1`.
//!
//! A `run-extension` plan step hands one mod to an extension built with this crate and compiled to
//! `wasm32-unknown-unknown`. The extension reads the mod, and where its declaration grants it the
//! game files and recorded installer answers, through the host's imports. Its only effect is the
//! operations it returns: files of the mod to place and text files to write. It has no filesystem,
//! network, clock or randomness; the host links only the imports the declaration grants and refuses
//! a module that asks for anything else, then checks every returned operation before it reaches a
//! deployment.
//!
//! ```ignore
//! struct MyInstaller;
//!
//! impl msbe_step_guest::Step for MyInstaller {
//!     fn run(context: &Context, request: Request) -> Result<Vec<Operation>, Error> {
//!         // Read context.entries(), ask questions, and return operations.
//!     }
//! }
//!
//! msbe_step_guest::export_step!(MyInstaller);
//! ```
//!
//! Off `wasm32`, [`Context::native`] serves a mod, game files and answers from memory, so a step's
//! logic can be unit tested natively. See `docs/18-wasm-extensions.md` for the ABI and packaging.

#[doc(hidden)]
pub mod abi;
mod context;
mod error;
mod records;

pub use context::{Context, Entry};
pub use error::Error;
pub use records::{Choice, Operation, Question, QuestionKind, Request};
pub use serde_json::{self, Value, json};

/// The step ABI version this crate implements.
pub const ABI_VERSION: i32 = 1;

/// A plan step implementation served from a sandbox.
pub trait Step {
    /// Works out what to install from one mod.
    ///
    /// The same mod, game files and answers must always produce the same operations: the host
    /// records what a generated file was derived from and relies on that to reproduce it.
    ///
    /// # Errors
    ///
    /// Returns an [`Error`] when the mod cannot be installed. [`Context::ask`] returns
    /// [`Error::QuestionPending`] for a question with no recorded answer and no default; the host
    /// then reports every such question to the user in place of the step's result.
    fn run(context: &Context, request: Request) -> Result<Vec<Operation>, Error>;
}

/// Exports `$step`, which implements [`Step`], through the `msbe-plan-step-1` ABI.
///
/// The exports exist only when compiling for `wasm32`, so a step crate also builds and tests
/// natively.
#[macro_export]
macro_rules! export_step {
    ($step:ty) => {
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
            extern "C" fn msbe_run(pointer: i32, length: i32) -> i64 {
                $crate::abi::run(pointer, length, <$step as $crate::Step>::run)
            }
        };
    };
}
