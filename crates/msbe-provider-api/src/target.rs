//! Provider-neutral compatibility facts and fail-closed availability rules.
//!
//! Every provider adapter receives the same [`Target`]. Adapters translate their native
//! metadata into this model before they select or return a candidate. A provider that omits
//! side metadata is incompatible by default; callers must never infer support from absence.

use msbe_plan_schema::Side;
use serde::{Deserialize, Serialize};

/// What a mod must be compatible with before it can be selected.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Target {
    /// The selected loader id.
    pub loader: String,
    /// Virtual loader APIs satisfied by the selected loader.
    pub provides: Vec<String>,
    /// The selected loader version, when the game or loader exposes one.
    pub loader_version: Option<String>,
    /// The game version, such as `1.21.1`.
    pub game_version: String,
    /// Whether this target is a player client or dedicated server.
    pub side: Side,
}

impl Target {
    /// Loader identifiers that can satisfy a provider's loader requirement.
    pub fn loader_ids(&self) -> impl Iterator<Item = &str> {
        std::iter::once(self.loader.as_str()).chain(self.provides.iter().map(String::as_str))
    }

    /// Whether metadata declared for each game side supports this target.
    pub const fn supports_side(&self, client: Availability, server: Availability) -> bool {
        match self.side {
            Side::Client => client.supports(),
            Side::Server => server.supports(),
        }
    }
}

/// Whether a provider declares a project available on one game side.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Availability {
    /// The project is required on this side.
    Required,
    /// The project may run on this side.
    Optional,
    /// The project cannot run on this side.
    Unsupported,
    /// The provider did not declare the project's availability.
    #[default]
    #[serde(other)]
    Unknown,
}

impl Availability {
    pub(crate) const fn supports(self) -> bool {
        matches!(self, Self::Required | Self::Optional)
    }
}
