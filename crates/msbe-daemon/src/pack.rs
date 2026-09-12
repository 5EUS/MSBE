//! Typed pack RPC: codec discovery, daemon-held previews, and job-backed execution (§17.11).
//!
//! Clients receive previews to display, never to submit. A preview is held here under a plan ID
//! and the digest of its content; executing names both, and the job re-plans from current state
//! and refuses to run unless the result is identical to what was previewed.

use std::{collections::BTreeMap, path::PathBuf};

use msbe_core::{
    config::Home,
    instance::{DEFAULT_PROFILE, Name},
};
use msbe_fsops::{Digest, RelPath};
use msbe_pack::{
    CapturePreview, CaptureRequest, Direction, ExportPreview, ExportRequest, ImportPreview,
    ImportRequest, IssueCode, PackError, Progress, Resolution, UpdatePreview, UpdateRequest,
};
use msbe_provider_api::{HttpClient, HttpError, PackOptions};
use msbe_providers::Providers;
use msbe_rpc_schema::{
    PACK_CAPTURE_EXECUTE_METHOD, PACK_CAPTURE_PREVIEW_METHOD, PACK_EXPORT_EXECUTE_METHOD,
    PACK_EXPORT_PREVIEW_METHOD, PACK_IMPORT_EXECUTE_METHOD, PACK_IMPORT_PREVIEW_METHOD,
    PACK_UPDATE_EXECUTE_METHOD, PACK_UPDATE_PREVIEW_METHOD, Response, SNAPSHOT_CREATE_METHOD,
    SNAPSHOT_RESTORE_METHOD, codes,
};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use serde_json::{Value, json};

use crate::jobs::{Environment, Jobs, Work};

/// How many previews the daemon holds before discarding the oldest.
const HELD_PLANS: usize = 64;

/// Why a typed request failed.
#[derive(Debug)]
pub(crate) enum Failure {
    /// The parameters are malformed.
    Params(String),
    /// A job holds the instance state.
    Busy,
    /// The pack operation failed.
    Pack(PackError),
}

impl From<PackError> for Failure {
    fn from(error: PackError) -> Self {
        Self::Pack(error)
    }
}

/// A preview the daemon holds until it is executed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Plan {
    Export(ExportPreview),
    Import(ImportPreview),
    Update(UpdatePreview),
    Capture(CapturePreview),
}

impl Plan {
    const fn execute_method(&self) -> &'static str {
        match self {
            Self::Export(_) => PACK_EXPORT_EXECUTE_METHOD,
            Self::Import(_) => PACK_IMPORT_EXECUTE_METHOD,
            Self::Update(_) => PACK_UPDATE_EXECUTE_METHOD,
            Self::Capture(_) => PACK_CAPTURE_EXECUTE_METHOD,
        }
    }

    fn to_value(&self) -> Result<Value, Failure> {
        match self {
            Self::Export(preview) => value(preview),
            Self::Import(preview) => value(preview),
            Self::Update(preview) => value(preview),
            Self::Capture(preview) => value(preview),
        }
        .map_err(Failure::from)
    }
}

/// Previews awaiting execution, by plan number.
#[derive(Debug, Default)]
pub(crate) struct Plans {
    next: u64,
    held: BTreeMap<u64, (Plan, Digest)>,
}

impl Plans {
    /// Holds `plan` and returns what a client displays: `{ plan_id, plan_digest, plan }`.
    pub(crate) fn hold(&mut self, plan: Plan) -> Result<Value, Failure> {
        let shown = plan.to_value()?;
        let digest = msbe_pack::plan_digest(&shown)?;
        self.next += 1;
        self.held.insert(self.next, (plan, digest));
        while self.held.len() > HELD_PLANS {
            self.held.pop_first();
        }
        Ok(json!({
            "plan_id": format!("plan-{}", self.next),
            "plan_digest": digest.to_string(),
            "plan": shown,
        }))
    }

    /// Releases the plan `id` for `method`, once, if `digest` names exactly what was previewed.
    fn take(&mut self, id: &str, digest: &str, method: &str) -> Result<Plan, Failure> {
        let number = id
            .strip_prefix("plan-")
            .and_then(|number| number.parse::<u64>().ok())
            .filter(|number| self.held.contains_key(number))
            .ok_or_else(|| stale(format!("plan {id} is unknown, expired or already executed")))?;
        let matches = self
            .held
            .get(&number)
            .is_some_and(|(_, held)| held.to_string() == digest);
        if !matches {
            return Err(stale(format!("{digest} is not the digest of plan {id}")));
        }
        if let Some((plan, _)) = self.held.get(&number)
            && plan.execute_method() != method
        {
            return Err(Failure::Params(format!(
                "plan {id} is executed with {}",
                plan.execute_method()
            )));
        }
        self.held
            .remove(&number)
            .map(|(plan, _)| plan)
            .ok_or_else(|| stale(format!("plan {id} is unknown")))
    }
}

/// Turns a typed result into a JSON-RPC response.
pub(crate) fn respond(id: Value, result: Result<Value, Failure>) -> Response {
    match result {
        Ok(value) => Response::success(id, value),
        Err(Failure::Params(message)) => Response::error(id, -32602, message),
        Err(Failure::Busy) => {
            Response::error(id, codes::BUSY, "a job is running; retry when it finishes")
        }
        Err(Failure::Pack(error)) => Response::error_with_data(
            id,
            codes::PACK,
            error.to_string(),
            Some(json!({
                "code": format!("{:?}", error.code()),
                "issues": serde_json::to_value(error.issues()).unwrap_or(Value::Null),
            })),
        ),
    }
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct CodecList {
    #[serde(default)]
    direction: Option<Direction>,
    #[serde(default)]
    game: Option<String>,
}

/// `pack.codec.list`.
pub(crate) fn codec_list(params: &Value) -> Result<Value, Failure> {
    let list: CodecList = optional(params)?;
    let providers = Providers::builtins().map_err(PackError::from)?;
    Ok(value(&msbe_pack::codecs(
        &providers,
        list.direction,
        list.game.as_deref(),
    ))?)
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct CodecOptions {
    codec: String,
    #[serde(default)]
    preset: Option<String>,
    #[serde(default)]
    direction: Option<Direction>,
}

/// `pack.codec.options`.
pub(crate) fn codec_options(params: &Value) -> Result<Value, Failure> {
    let request: CodecOptions = required(params)?;
    let providers = Providers::builtins().map_err(PackError::from)?;
    Ok(value(&msbe_pack::codec_options(
        &providers,
        &request.codec,
        request.direction.unwrap_or(Direction::Export),
        request.preset.as_deref(),
    )?)?)
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ExportParams {
    instance: Name,
    #[serde(default)]
    profile: Option<Name>,
    codec: String,
    #[serde(default)]
    preset: Option<String>,
    #[serde(default)]
    options: Value,
    output: PathBuf,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ImportParams {
    instance: Name,
    #[serde(default)]
    profile: Option<Name>,
    input: PathBuf,
    #[serde(default)]
    codec: Option<String>,
    #[serde(default)]
    options: Value,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct UpdateParams {
    instance: Name,
    #[serde(default)]
    profile: Option<Name>,
    input: PathBuf,
    #[serde(default)]
    codec: Option<String>,
    #[serde(default)]
    resolutions: BTreeMap<String, Resolution>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct CaptureParams {
    instance: Name,
    #[serde(default)]
    profile: Option<Name>,
    #[serde(default)]
    paths: Vec<RelPath>,
}

/// Plans the operation a preview method names.
pub(crate) fn preview(method: &str, params: &Value, home: &Home) -> Result<Plan, Failure> {
    let providers = Providers::builtins().map_err(PackError::from)?;
    match method {
        PACK_EXPORT_PREVIEW_METHOD => {
            let request: ExportParams = required(params)?;
            let options = msbe_pack::options_document(
                &providers,
                &request.codec,
                Direction::Export,
                &request.options,
            )?;
            let preview = msbe_pack::preview_export(
                &providers,
                home,
                ExportRequest {
                    instance: request.instance,
                    profile: profile(request.profile)?,
                    codec: request.codec,
                    preset: request.preset,
                    options,
                    output: absolute(request.output)?,
                },
            )?;
            Ok(Plan::Export(preview))
        }
        PACK_IMPORT_PREVIEW_METHOD => {
            let request: ImportParams = required(params)?;
            let options = match (&request.codec, &request.options) {
                (Some(codec), options) => {
                    msbe_pack::options_document(&providers, codec, Direction::Import, options)?
                }
                (None, Value::Null) => PackOptions::new(),
                (None, _) => {
                    return Err(Failure::Params(
                        "import options need a codec whose schema types them".to_owned(),
                    ));
                }
            };
            let preview = msbe_pack::preview_import(
                &providers,
                home,
                ImportRequest {
                    instance: request.instance,
                    profile: profile(request.profile)?,
                    input: absolute(request.input)?,
                    codec: request.codec,
                    options,
                },
            )?;
            Ok(Plan::Import(preview))
        }
        PACK_UPDATE_PREVIEW_METHOD => {
            let request: UpdateParams = required(params)?;
            let preview = msbe_pack::preview_update(
                &providers,
                home,
                UpdateRequest {
                    instance: request.instance,
                    profile: profile(request.profile)?,
                    input: absolute(request.input)?,
                    codec: request.codec,
                    resolutions: request.resolutions,
                },
            )?;
            Ok(Plan::Update(preview))
        }
        PACK_CAPTURE_PREVIEW_METHOD => {
            let request: CaptureParams = required(params)?;
            let preview = msbe_pack::preview_capture(
                home,
                CaptureRequest {
                    instance: request.instance,
                    profile: profile(request.profile)?,
                    paths: request.paths,
                },
            )?;
            Ok(Plan::Capture(preview))
        }
        other => Err(Failure::Params(format!("{other} is not a preview method"))),
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Start {
    method: String,
    #[serde(default)]
    params: Value,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Execute {
    plan_id: String,
    plan_digest: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct SnapshotCreate {
    instance: Name,
    output: PathBuf,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct SnapshotRestore {
    input: PathBuf,
}

/// Validates a `job.start` request and returns the job method and its work.
pub(crate) fn start(plans: &mut Plans, params: &Value) -> Result<(String, Work), Failure> {
    let start: Start = required(params)?;
    let work = match start.method.as_str() {
        PACK_IMPORT_EXECUTE_METHOD
        | PACK_UPDATE_EXECUTE_METHOD
        | PACK_EXPORT_EXECUTE_METHOD
        | PACK_CAPTURE_EXECUTE_METHOD => {
            let execute: Execute = required(&start.params)?;
            Work::Plan(Box::new(plans.take(
                &execute.plan_id,
                &execute.plan_digest,
                &start.method,
            )?))
        }
        SNAPSHOT_CREATE_METHOD => {
            let request: SnapshotCreate = required(&start.params)?;
            Work::SnapshotCreate {
                instance: request.instance,
                output: absolute(request.output)?,
            }
        }
        SNAPSHOT_RESTORE_METHOD => {
            let request: SnapshotRestore = required(&start.params)?;
            Work::SnapshotRestore {
                input: absolute(request.input)?,
            }
        }
        other => return Err(Failure::Params(format!("{other} is not a job method"))),
    };
    Ok((start.method, work))
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Events {
    job_id: u64,
    #[serde(default)]
    after: u64,
}

/// `job.events`.
pub(crate) fn job_events(jobs: &Jobs, params: &Value) -> Result<Value, Failure> {
    let request: Events = required(params)?;
    let status = jobs
        .status(request.job_id, request.after)
        .ok_or_else(|| Failure::Params(format!("there is no job {}", request.job_id)))?;
    Ok(value(&status)?)
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Cancel {
    job_id: u64,
}

/// `job.cancel`.
pub(crate) fn job_cancel(jobs: &Jobs, params: &Value) -> Result<Value, Failure> {
    let request: Cancel = required(params)?;
    let cancelled = jobs
        .cancel(request.job_id)
        .ok_or_else(|| Failure::Params(format!("there is no job {}", request.job_id)))?;
    Ok(json!({ "cancelled": cancelled }))
}

/// Runs a job. A held plan is planned again from current state first and runs only if nothing it
/// relied on changed.
pub(crate) fn execute(
    work: Work,
    environment: &Environment,
    progress: &dyn Progress,
) -> Result<Value, PackError> {
    let home = match &environment.home {
        Some(home) => Home::at(home),
        None => Home::discover()?,
    };
    let providers = Providers::builtins()?;
    let connect: &dyn Fn() -> Result<Box<dyn HttpClient>, HttpError> = &*environment.connect;
    match work {
        Work::Plan(plan) => match *plan {
            Plan::Export(held) => {
                unchanged(
                    &msbe_pack::preview_export(&providers, &home, held.request.clone())?,
                    &held,
                )?;
                value(&msbe_pack::execute_export(
                    &providers, &home, &held, progress,
                )?)
            }
            Plan::Import(held) => {
                unchanged(
                    &msbe_pack::preview_import(&providers, &home, held.request.clone())?,
                    &held,
                )?;
                value(&msbe_pack::execute_import(
                    &providers, &home, &held, connect, progress,
                )?)
            }
            Plan::Update(held) => {
                unchanged(
                    &msbe_pack::preview_update(&providers, &home, held.request.clone())?,
                    &held,
                )?;
                value(&msbe_pack::execute_update(
                    &providers, &home, &held, connect, progress,
                )?)
            }
            Plan::Capture(held) => {
                unchanged(
                    &msbe_pack::preview_capture(&home, held.request.clone())?,
                    &held,
                )?;
                value(&msbe_pack::execute_capture(&home, &held, progress)?)
            }
        },
        Work::SnapshotCreate { instance, output } => value(&msbe_pack::create_snapshot(
            &home, &instance, &output, progress,
        )?),
        Work::SnapshotRestore { input } => {
            let preview = msbe_pack::preview_restore(&home, &input)?;
            value(&msbe_pack::restore_snapshot(&home, &preview, progress)?)
        }
    }
}

fn unchanged<T: PartialEq>(current: &T, held: &T) -> Result<(), PackError> {
    if current == held {
        Ok(())
    } else {
        Err(PackError::issue(
            IssueCode::StalePlan,
            "the profile, codec, options, inputs or observations changed after the preview; preview again",
        ))
    }
}

fn stale(message: String) -> Failure {
    Failure::Pack(PackError::issue(IssueCode::StalePlan, message))
}

fn profile(profile: Option<Name>) -> Result<Name, Failure> {
    match profile {
        Some(profile) => Ok(profile),
        None => Name::new(DEFAULT_PROFILE).map_err(|error| Failure::Pack(error.into())),
    }
}

/// The daemon's working directory is not the client's, so paths must be absolute.
fn absolute(path: PathBuf) -> Result<PathBuf, Failure> {
    if path.is_absolute() {
        Ok(path)
    } else {
        Err(Failure::Params(format!(
            "{} must be an absolute path",
            path.display()
        )))
    }
}

fn required<T: DeserializeOwned>(params: &Value) -> Result<T, Failure> {
    serde_json::from_value(params.clone()).map_err(|error| Failure::Params(error.to_string()))
}

fn optional<T: DeserializeOwned + Default>(params: &Value) -> Result<T, Failure> {
    if params.is_null() {
        Ok(T::default())
    } else {
        required(params)
    }
}

fn value<T: Serialize>(report: &T) -> Result<Value, PackError> {
    serde_json::to_value(report)
        .map_err(|error| PackError::issue(IssueCode::HostFailure, error.to_string()))
}
