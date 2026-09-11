//! Modrinth's metadata API, through one bounded endpoint.

use std::collections::BTreeMap;

use msbe_provider_api::{AdapterError, HttpClient, JsonEndpoint, Target};
use serde::{Serialize, de::DeserializeOwned};

use crate::{
    reference::segment,
    wire::{HashQuery, Project, SearchHit, SearchResults, Version},
};

/// The most bytes a metadata response may be.
const METADATA_LIMIT: u64 = 16 << 20;

/// The most search results Modrinth returns in one request.
const MAX_SEARCH_LIMIT: u8 = 100;

/// Requests against one deployment of Modrinth's API.
pub(crate) struct Client<'a> {
    endpoint: JsonEndpoint<'a>,
}

impl<'a> Client<'a> {
    pub(crate) fn new(http: &'a dyn HttpClient, base: &str) -> Self {
        Self {
            endpoint: JsonEndpoint::new(http, base, METADATA_LIMIT),
        }
    }

    /// Searches for mods, with the target's game version and loaders as facets.
    pub(crate) fn search(
        &self,
        query: &str,
        target: &Target,
        limit: u8,
    ) -> Result<Vec<SearchHit>, AdapterError> {
        let mut facets = vec![
            vec!["project_type:mod".to_owned()],
            vec![format!("versions:{}", target.game_version)],
        ];
        let loader_ids: Vec<&str> = target.loader_ids().collect();
        if !loader_ids.is_empty() {
            facets.push(
                loader_ids
                    .iter()
                    .map(|loader| format!("categories:{loader}"))
                    .collect(),
            );
        }
        let facets = serde_json::to_string(&facets).unwrap_or_default();
        let limit = limit.min(MAX_SEARCH_LIMIT).to_string();
        let results: SearchResults = self.get(
            "/search",
            &[("query", query), ("facets", &facets), ("limit", &limit)],
        )?;
        Ok(results.hits)
    }

    /// Fetches a project by slug or id.
    pub(crate) fn project(&self, reference: &str) -> Result<Project, AdapterError> {
        self.get(&format!("/project/{}", segment(reference)?), &[])
    }

    /// Lists a project's versions for the target's loaders and game version.
    pub(crate) fn versions(
        &self,
        project: &str,
        target: &Target,
    ) -> Result<Vec<Version>, AdapterError> {
        let loader_ids: Vec<&str> = target.loader_ids().collect();
        let loaders = serde_json::to_string(&loader_ids).unwrap_or_default();
        let game_versions =
            serde_json::to_string(std::slice::from_ref(&target.game_version)).unwrap_or_default();
        self.get(
            &format!("/project/{}/version", segment(project)?),
            &[
                ("loaders", &loaders),
                ("game_versions", &game_versions),
                ("include_changelog", "false"),
            ],
        )
    }

    /// Fetches one version by id.
    pub(crate) fn version(&self, id: &str) -> Result<Version, AdapterError> {
        self.get(&format!("/version/{}", segment(id)?), &[])
    }

    /// The versions the queried files belong to, keyed by hash.
    pub(crate) fn versions_by_hash(
        &self,
        query: &HashQuery<'_>,
    ) -> Result<BTreeMap<String, Version>, AdapterError> {
        self.post("/version_files", query)
    }

    /// The newest version the query admits of each queried file's project, keyed by hash.
    pub(crate) fn latest_by_hash(
        &self,
        query: &HashQuery<'_>,
    ) -> Result<BTreeMap<String, Version>, AdapterError> {
        self.post("/version_files/update", query)
    }

    fn get<T: DeserializeOwned>(
        &self,
        path: &str,
        query: &[(&str, &str)],
    ) -> Result<T, AdapterError> {
        Ok(self.endpoint.get(path, query)?)
    }

    fn post<T: DeserializeOwned>(
        &self,
        path: &str,
        request: &impl Serialize,
    ) -> Result<T, AdapterError> {
        Ok(self.endpoint.post(path, request)?)
    }
}
