//! Provider-neutral metadata records returned by reviewed adapter operations.

use serde::Serialize;

/// A content project found by a provider search.
///
/// Provider adapters translate their native result objects into this stable record before
/// returning it to command and UI layers. Provider-specific fields remain within the adapter.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SearchResult {
    /// The stable provider identifier.
    pub provider: String,
    /// The provider's stable project identifier.
    pub project: String,
    /// The provider-specific project reference suitable for a subsequent selection.
    #[serde(rename = "slug")]
    pub reference: String,
    /// The display title.
    pub title: String,
    /// A short provider-supplied summary.
    pub description: String,
    /// Provider-reported popularity, when available.
    pub downloads: u64,
}
