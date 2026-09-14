//! Typed methods over one instance's state: its deployment journal, deploy conflicts, and the
//! provider update preview (`docs/03-architecture.md` §3.4). The caller holds the instance-state
//! lock.

use msbe_core::{
    config::Home,
    instance::{DEFAULT_PROFILE, Instance, Name},
};
use msbe_fsops::TxnId;
use msbe_pack::{IssueCode, PackError};
use msbe_providers::Providers;
use msbe_rpc_schema::{InstanceRequest, JournalRollback, ProfileRequest};
use serde::de::DeserializeOwned;
use serde_json::{Value, json};

use crate::pack::Failure;

fn typed<T: DeserializeOwned>(params: &Value) -> Result<T, Failure> {
    serde_json::from_value(params.clone()).map_err(|error| Failure::Params(error.to_string()))
}

fn name(raw: &str) -> Result<Name, Failure> {
    Name::new(raw).map_err(|error| Failure::Pack(PackError::from(error)))
}

fn open(home: &Home, instance: &str) -> Result<Instance, Failure> {
    Instance::open(home, &name(instance)?).map_err(|error| Failure::Pack(PackError::from(error)))
}

/// `journal.list`.
pub(crate) fn journal_list(home: &Home, params: &Value) -> Result<Value, Failure> {
    let request: InstanceRequest = typed(params)?;
    Ok(json!(open(home, &request.instance)?.journal()))
}

/// `journal.rollback`.
pub(crate) fn journal_rollback(home: &Home, params: &Value) -> Result<Value, Failure> {
    let request: JournalRollback = typed(params)?;
    let mut instance = open(home, &request.instance)?;
    let undone = match request.txn {
        Some(txn) => instance.rollback_to(TxnId::new(txn)),
        None => instance
            .rollback()
            .map(|undone| undone.into_iter().collect()),
    }
    .map_err(|error| Failure::Pack(PackError::from(error)))?;
    Ok(json!({ "rolled_back": undone, "journal": instance.journal() }))
}

/// `conflicts.list`.
pub(crate) fn conflicts(home: &Home, params: &Value) -> Result<Value, Failure> {
    let request: ProfileRequest = typed(params)?;
    let instance = open(home, &request.instance)?;
    let profile = name(request.profile.as_deref().unwrap_or(DEFAULT_PROFILE))?;
    let found = instance
        .conflicts(&profile)
        .map_err(|error| Failure::Pack(PackError::from(error)))?;
    Ok(json!(found))
}

/// `update.preview`.
pub(crate) fn update_preview(
    home: &Home,
    params: &Value,
    connect: msbe_cli::Connect<'_>,
) -> Result<Value, Failure> {
    let request: ProfileRequest = typed(params)?;
    let providers =
        Providers::installed(home).map_err(|error| Failure::Pack(PackError::from(error)))?;
    msbe_cli::update_report(
        &providers,
        home,
        &request.instance,
        request.profile.as_deref().unwrap_or(DEFAULT_PROFILE),
        &request.modules,
        true,
        connect,
    )
    .map_err(|message| Failure::Pack(PackError::issue(IssueCode::HostFailure, message)))
}
