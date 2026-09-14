//! The programs users registered for tool providers, each pinned by its SHA-256
//! (`docs/06-providers-and-policy.md` §6.5). Shared by `msbe tool`, the daemon's `tool.*` methods, and
//! the daemon's runner, which runs a program only while it still has the SHA-256 it was registered
//! with.
//!
//! ```text
//! <home>/tools/tools.toml    [[tool]] provider, program, sha256, registered
//! ```

use std::{
    fs::{self, File},
    io::{self, Read as _},
    path::{Path, PathBuf},
};

use msbe_core::config::Home;
use msbe_provider_api::{Provider, hex};
use msbe_providers::{Providers, RegistryError};
use msbe_rpc_schema::{ToolState, ToolStatus};
use msbe_secrets::{Acknowledgements, Clock, StoreError};
use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};

/// The most bytes the registration file may be.
const FILE_LIMIT: u64 = 1 << 20;

/// A program registered for a tool provider.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ToolRegistration {
    /// The tool provider.
    pub provider: String,
    /// The program, with every link resolved.
    pub program: PathBuf,
    /// Its SHA-256 when it was registered, as lowercase hex.
    pub sha256: String,
    /// When it was registered, in Unix seconds.
    pub registered: u64,
}

#[derive(Debug, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Document {
    #[serde(default, rename = "tool")]
    tools: Vec<ToolRegistration>,
}

/// Why a tool registration could not be read or changed.
#[derive(Debug, thiserror::Error)]
pub enum ToolsError {
    /// No enabled provider with this id runs a tool.
    #[error("no enabled provider named {0:?} runs an external tool")]
    UnknownTool(String),
    /// Registering accepts the provider's terms, and they were not accepted.
    #[error(
        "registering a program for {provider} accepts its terms at {terms}; read them, then pass --accept-terms"
    )]
    TermsNotAccepted {
        /// The provider.
        provider: String,
        /// Its terms.
        terms: String,
    },
    /// The program cannot be registered.
    #[error("cannot register {}: {reason}", .path.display())]
    Program {
        /// The program.
        path: PathBuf,
        /// Why not.
        reason: String,
    },
    /// The registration file cannot be read or written.
    #[error("cannot {action} {}: {source}", .path.display())]
    Io {
        /// What was being done.
        action: &'static str,
        /// The file.
        path: PathBuf,
        /// The failure.
        #[source]
        source: io::Error,
    },
    /// The registration file is not one MSBE wrote.
    #[error("{} is not a tool registration file: {reason}", .path.display())]
    Malformed {
        /// The file.
        path: PathBuf,
        /// What is wrong with it.
        reason: String,
    },
    /// The installed providers cannot be read.
    #[error(transparent)]
    Registry(#[from] RegistryError),
    /// The acknowledgement cannot be recorded.
    #[error(transparent)]
    Secrets(#[from] StoreError),
    /// The registration file cannot be replaced.
    #[error(transparent)]
    Write(#[from] msbe_fsops::Error),
}

/// The file tool registrations are kept in.
pub fn tools_file(home: &Home) -> PathBuf {
    home.root().join("tools").join("tools.toml")
}

/// Every registration, ordered by provider.
///
/// # Errors
///
/// Returns [`ToolsError`] when the file cannot be read or is not one MSBE wrote.
pub fn tool_registrations(home: &Home) -> Result<Vec<ToolRegistration>, ToolsError> {
    let path = tools_file(home);
    let file = match File::open(&path) {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(source) => {
            return Err(ToolsError::Io {
                action: "read",
                path,
                source,
            });
        }
    };
    let mut text = String::new();
    file.take(FILE_LIMIT + 1)
        .read_to_string(&mut text)
        .map_err(|source| ToolsError::Io {
            action: "read",
            path: path.clone(),
            source,
        })?;
    if text.len() as u64 > FILE_LIMIT {
        return Err(ToolsError::Malformed {
            path,
            reason: "it is larger than 1 MiB".to_owned(),
        });
    }
    toml::from_str::<Document>(&text)
        .map(|document| document.tools)
        .map_err(|error| ToolsError::Malformed {
            path,
            reason: error.to_string(),
        })
}

/// The registration for `provider`, if it has one.
///
/// # Errors
///
/// As for [`tool_registrations`].
pub fn tool_registration(
    home: &Home,
    provider: &str,
) -> Result<Option<ToolRegistration>, ToolsError> {
    Ok(tool_registrations(home)?
        .into_iter()
        .find(|registration| registration.provider == provider))
}

/// The SHA-256 of the program at `path`, as lowercase hex.
///
/// # Errors
///
/// Returns an error when the program cannot be read.
pub fn program_sha256(path: &Path) -> io::Result<String> {
    let mut file = File::open(path)?;
    let mut hasher = Sha256::new();
    let mut buffer = vec![0_u8; 1 << 16];
    loop {
        let read = file.read(&mut buffer)?;
        let Some(chunk) = buffer.get(..read).filter(|chunk| !chunk.is_empty()) else {
            break;
        };
        hasher.update(chunk);
    }
    Ok(hex(&hasher.finalize()))
}

/// Every enabled tool provider, and the program registered for each.
///
/// # Errors
///
/// As for [`tool_registrations`].
pub fn tool_statuses(providers: &Providers, home: &Home) -> Result<Vec<ToolStatus>, ToolsError> {
    let registrations = tool_registrations(home)?;
    Ok(providers
        .tool_providers()
        .into_iter()
        .map(|provider| {
            let registration = registrations
                .iter()
                .find(|registration| registration.provider == provider.id);
            status(provider, registration)
        })
        .collect())
}

/// Registers `program` for the tool provider `provider`, pinning its SHA-256, and records that the
/// provider's terms were accepted. It replaces any program registered for the provider before.
///
/// # Errors
///
/// Returns [`ToolsError::UnknownTool`] for a provider that runs no tool,
/// [`ToolsError::TermsNotAccepted`] unless `accept_terms`, [`ToolsError::Program`] for a path that is
/// not absolute or not a readable file, and [`ToolsError`] when state cannot be written.
pub fn register_tool(
    providers: &Providers,
    home: &Home,
    provider: &str,
    program: &Path,
    accept_terms: bool,
    clock: &dyn Clock,
) -> Result<ToolStatus, ToolsError> {
    let found = tool_provider(providers, provider)?;
    if !accept_terms {
        return Err(ToolsError::TermsNotAccepted {
            provider: provider.to_owned(),
            terms: found.policy.tos_url.clone(),
        });
    }
    let refuse = |reason: String| ToolsError::Program {
        path: program.to_path_buf(),
        reason,
    };
    if !program.is_absolute() {
        return Err(refuse("the path is not absolute".to_owned()));
    }
    let resolved = fs::canonicalize(program).map_err(|error| refuse(error.to_string()))?;
    if !resolved.is_file() {
        return Err(refuse("it is not a file".to_owned()));
    }
    let sha256 = program_sha256(&resolved).map_err(|error| refuse(error.to_string()))?;
    Acknowledgements::load(home)?.acknowledge(
        provider,
        &found.policy.tos_url,
        providers.program_digest(provider),
        clock,
    )?;
    let mut registrations = tool_registrations(home)?;
    registrations.retain(|registration| registration.provider != provider);
    registrations.push(ToolRegistration {
        provider: provider.to_owned(),
        program: resolved,
        sha256,
        registered: clock.now(),
    });
    registrations.sort_by(|left, right| left.provider.cmp(&right.provider));
    save(home, &registrations)?;
    let registration = registrations
        .iter()
        .find(|registration| registration.provider == provider);
    Ok(status(found, registration))
}

/// Forgets the program registered for `provider`. A provider that no longer runs a tool can still be
/// forgotten.
///
/// # Errors
///
/// Returns [`ToolsError::UnknownTool`] for a provider that neither runs a tool nor has a
/// registration, and [`ToolsError`] when state cannot be read or written.
pub fn forget_tool(
    providers: &Providers,
    home: &Home,
    provider: &str,
) -> Result<ToolStatus, ToolsError> {
    let mut registrations = tool_registrations(home)?;
    let registered = registrations
        .iter()
        .any(|registration| registration.provider == provider);
    let found = tool_provider(providers, provider);
    if !registered && found.is_err() {
        return Err(ToolsError::UnknownTool(provider.to_owned()));
    }
    registrations.retain(|registration| registration.provider != provider);
    save(home, &registrations)?;
    Ok(match found {
        Ok(found) => status(found, None),
        Err(_) => ToolStatus {
            provider: provider.to_owned(),
            name: provider.to_owned(),
            terms: String::new(),
            program: None,
            sha256: None,
            state: ToolState::Unregistered,
        },
    })
}

fn tool_provider<'a>(providers: &'a Providers, provider: &str) -> Result<&'a Provider, ToolsError> {
    providers
        .tool_providers()
        .into_iter()
        .find(|found| found.id == provider)
        .ok_or_else(|| ToolsError::UnknownTool(provider.to_owned()))
}

fn status(provider: &Provider, registration: Option<&ToolRegistration>) -> ToolStatus {
    let state = registration.map_or(
        ToolState::Unregistered,
        |registration| match program_sha256(&registration.program) {
            Ok(found) if found == registration.sha256 => ToolState::Registered,
            Ok(_) => ToolState::Changed,
            Err(_) => ToolState::Missing,
        },
    );
    ToolStatus {
        provider: provider.id.clone(),
        name: provider.name.clone(),
        terms: provider.policy.tos_url.clone(),
        program: registration.map(|registration| registration.program.display().to_string()),
        sha256: registration.map(|registration| registration.sha256.clone()),
        state,
    }
}

fn save(home: &Home, registrations: &[ToolRegistration]) -> Result<(), ToolsError> {
    let path = tools_file(home);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|source| ToolsError::Io {
            action: "create",
            path: parent.to_path_buf(),
            source,
        })?;
    }
    let text = toml::to_string(&Document {
        tools: registrations.to_vec(),
    })
    .map_err(|error| ToolsError::Malformed {
        path: path.clone(),
        reason: error.to_string(),
    })?;
    msbe_fsops::atomic::write_file(&path, text.as_bytes())?;
    Ok(())
}
