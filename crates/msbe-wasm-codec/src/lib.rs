//! Sandboxed WebAssembly runtime for MSBE pack codecs.
//!
//! The runtime links only the bounded `msbe_input` interface. A module requesting filesystem,
//! network, clock, randomness, or any other host import is rejected before it can run. Every call
//! runs in a fresh instance with fuel, memory and read limits, and reads its pack lazily: an entry
//! the codec never asks for is never read. See `docs/18-wasm-extensions.md` §18.3.

#![forbid(unsafe_code)]

use std::{
    collections::BTreeMap,
    sync::{Mutex, MutexGuard, OnceLock, PoisonError, mpsc},
    thread,
};

use msbe_fsops::{Digest, RelPath};
use msbe_provider_api::{
    ContainerKind, ExtensionEnvelope, ExtensionProvide, PackCodec, PackCodecDescriptor,
    PackCodecError, PackEntry, PackExportContext, PackExportPlan, PackImportContext,
    PackImportPlan, PackInput, PackLayout, PackOptions, PackProbe, VerifyingKey,
};
use serde::de::DeserializeOwned;
use serde_json::{Value, json};
use wasmtime::{
    Caller, Config, Engine, ExternType, Instance, Linker, Module, Store, StoreLimits,
    StoreLimitsBuilder,
};

const ABI_VERSION: i32 = 1;
const HOST_API_VERSION: u32 = 1;
/// Fuel for one call. It bounds how long a call runs, the same on every machine, and is enough to
/// parse indexes and lockfiles of several megabytes.
const FUEL: u64 = 10_000_000_000;
/// The most linear memory one call's instance may grow to.
const MEMORY_LIMIT: usize = 512 << 20;
/// The most pack bytes one call may read.
const READ_BUDGET: u64 = 64 << 20;
const NOT_FOUND: i64 = -1;
const LIMIT: i64 = -2;
const UNSAFE_PATH: i64 = -3;
const BUDGET: i64 = -4;
const INPUT_FAILED: i64 = -5;

/// The engine every codec in this process is compiled for.
static ENGINE: OnceLock<Engine> = OnceLock::new();

/// Codecs compiled in this process, with their descriptors, by module digest.
type Compiled = BTreeMap<Digest, (Module, PackCodecDescriptor)>;

/// Compiling dominates the cost of loading a codec, and hosts build their registry for every
/// command, so each module is compiled and asked for its descriptor once per process.
static COMPILED: Mutex<Compiled> = Mutex::new(BTreeMap::new());

/// A compiled WebAssembly pack codec served through the neutral codec contract.
#[derive(Debug)]
pub struct WasmPackCodec {
    engine: Engine,
    module: Module,
    descriptor: PackCodecDescriptor,
}

impl WasmPackCodec {
    /// Verifies and loads a pure WASM codec extension envelope.
    ///
    /// # Errors
    ///
    /// Returns an error when the extension is untrusted, incompatible, requests capabilities, or
    /// its module fails the normal codec validation.
    pub fn load_signed(
        envelope: &ExtensionEnvelope<Vec<u8>>,
        trusted_keys: &BTreeMap<String, VerifyingKey>,
    ) -> Result<Self, PackCodecError> {
        envelope.verify(trusted_keys).map_err(runtime_error)?;
        if envelope.provides != [ExtensionProvide::PackCodecV1] {
            return Err(PackCodecError::Codec(
                "WASM codec must provide exactly pack-codec-v1".to_owned(),
            ));
        }
        if !envelope.capabilities.is_empty() {
            return Err(PackCodecError::Codec(
                "WASM codecs may not request capabilities".to_owned(),
            ));
        }
        if !(envelope.host_api.minimum..=envelope.host_api.maximum).contains(&HOST_API_VERSION) {
            return Err(PackCodecError::Codec(format!(
                "WASM codec host API {HOST_API_VERSION} is unsupported"
            )));
        }
        Self::load(&envelope.payload)
    }

    /// Compiles and validates a codec module, then obtains its descriptor through the sandbox.
    ///
    /// # Errors
    ///
    /// Returns an error when the module asks for imports outside `msbe_input`, has an incompatible
    /// ABI version, or returns an invalid descriptor.
    pub fn load(bytes: &[u8]) -> Result<Self, PackCodecError> {
        let engine = shared_engine()?;
        let digest = Digest::of_bytes(bytes);
        if let Some((module, descriptor)) = compiled().get(&digest) {
            return Ok(Self {
                engine,
                module: module.clone(),
                descriptor: descriptor.clone(),
            });
        }
        let module = Module::new(&engine, bytes).map_err(runtime_error)?;
        validate_imports(&module)?;
        let response = run(&engine, &module, &EmptyInput, |store, instance| {
            let version = instance
                .get_typed_func::<(), i32>(&mut *store, "msbe_abi_version")
                .map_err(runtime_error)?
                .call(&mut *store, ())
                .map_err(runtime_error)?;
            if version != ABI_VERSION {
                return Err(PackCodecError::Codec(format!(
                    "unsupported WASM codec ABI version {version}"
                )));
            }
            call_without_request(store, instance, "msbe_descriptor")
        })?;
        let descriptor: PackCodecDescriptor = record(response)?;
        descriptor.validate()?;
        compiled().insert(digest, (module.clone(), descriptor.clone()));
        Ok(Self {
            engine,
            module,
            descriptor,
        })
    }

    /// Calls `function`, with `request` when it takes one, serving `input`, and decodes the record
    /// it returns.
    fn call<T: DeserializeOwned>(
        &self,
        input: &dyn PackInput,
        function: &str,
        request: Option<&Value>,
    ) -> Result<T, PackCodecError> {
        record(run(
            &self.engine,
            &self.module,
            input,
            |store, instance| match request {
                Some(request) => call_with_request(store, instance, function, request),
                None => call_without_request(store, instance, function),
            },
        )?)
    }
}

impl PackCodec for WasmPackCodec {
    fn descriptor(&self) -> &PackCodecDescriptor {
        &self.descriptor
    }

    fn probe(&self, input: &dyn PackInput) -> Result<PackProbe, PackCodecError> {
        self.call(input, "msbe_probe", None)
    }

    fn plan_import(
        &self,
        input: &dyn PackInput,
        context: &PackImportContext,
        options: &PackOptions,
    ) -> Result<PackImportPlan, PackCodecError> {
        self.call(
            input,
            "msbe_plan_import",
            Some(&json!({ "context": context, "options": options })),
        )
    }

    fn plan_export(
        &self,
        context: &PackExportContext<'_>,
        options: &PackOptions,
    ) -> Result<PackExportPlan, PackCodecError> {
        self.call(
            &EmptyInput,
            "msbe_plan_export",
            Some(&json!({
                "context": {
                    "game": context.game,
                    "target": context.target,
                    "lockfile": context.lockfile,
                    "files": context.files,
                    "observations": context.observations,
                    "inclusion": context.inclusion,
                },
                "options": options,
            })),
        )
    }

    fn layout(&self, plan: &PackExportPlan) -> Result<PackLayout, PackCodecError> {
        self.call(
            &EmptyInput,
            "msbe_layout",
            Some(&serde_json::to_value(plan).map_err(runtime_error)?),
        )
    }
}

/// What one call's instance can reach: the pack's framing and entries, a channel to the thread
/// that reads entries, and the call's limits.
#[derive(Debug)]
struct HostState {
    container: ContainerKind,
    /// The entries, as the JSON the `entries` import stages.
    entries: Vec<u8>,
    reads: mpsc::Sender<(String, u64)>,
    answers: mpsc::Receiver<Result<Vec<u8>, i64>>,
    staged: Vec<u8>,
    read_left: u64,
    limits: StoreLimits,
}

#[derive(Debug)]
struct EmptyInput;

impl PackInput for EmptyInput {
    fn container(&self) -> ContainerKind {
        ContainerKind::File
    }

    fn entries(&self) -> &[PackEntry] {
        &[]
    }

    fn read(&self, _: &RelPath, _: u64) -> Result<Vec<u8>, PackCodecError> {
        Err(PackCodecError::FormatMismatch)
    }
}

/// An engine whose runs give identical output for identical input: fuel-metered, with no
/// shared-memory threads, and relaxed SIMD results and `NaN` bit patterns fixed across machines.
fn engine() -> Result<Engine, PackCodecError> {
    let mut config = Config::new();
    config.consume_fuel(true);
    config.wasm_threads(false);
    config.relaxed_simd_deterministic(true);
    config.cranelift_nan_canonicalization(true);
    Engine::new(&config).map_err(runtime_error)
}

/// The process's codec engine, created on first use.
fn shared_engine() -> Result<Engine, PackCodecError> {
    if let Some(engine) = ENGINE.get() {
        return Ok(engine.clone());
    }
    let engine = engine()?;
    Ok(ENGINE.get_or_init(|| engine).clone())
}

/// The compiled-codec cache. A panic while it was held cannot leave an entry half-written, so a
/// poisoned lock is still usable.
fn compiled() -> MutexGuard<'static, Compiled> {
    COMPILED.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Runs `body` against a fresh instance of `module` on its own thread, while this thread serves
/// the reads it makes of `input`. The pack never leaves its owner, and only what the codec asks for
/// is read.
fn run<T: Send>(
    engine: &Engine,
    module: &Module,
    input: &dyn PackInput,
    body: impl FnOnce(&mut Store<HostState>, &Instance) -> Result<T, PackCodecError> + Send,
) -> Result<T, PackCodecError> {
    let entries = serde_json::to_vec(input.entries()).map_err(runtime_error)?;
    let container = input.container();
    let (reads, requests) = mpsc::channel::<(String, u64)>();
    let (answer, answers) = mpsc::channel::<Result<Vec<u8>, i64>>();
    thread::scope(|scope| {
        let call = scope.spawn(move || {
            let mut store = Store::new(
                engine,
                HostState {
                    container,
                    entries,
                    reads,
                    answers,
                    staged: Vec::new(),
                    read_left: READ_BUDGET,
                    limits: StoreLimitsBuilder::new()
                        .memory_size(MEMORY_LIMIT)
                        .memories(1)
                        .tables(16)
                        .instances(1)
                        .build(),
                },
            );
            store.limiter(|state| &mut state.limits);
            store.set_fuel(FUEL).map_err(runtime_error)?;
            let instance = instantiate(&mut store, module)?;
            body(&mut store, &instance)
        });
        // Ends when the call finishes and its store, holding the only sender, is dropped.
        for (path, limit) in requests {
            if answer.send(read_entry(input, &path, limit)).is_err() {
                break;
            }
        }
        call.join().unwrap_or_else(|_| {
            Err(PackCodecError::Codec(
                "the WASM codec call stopped unexpectedly".to_owned(),
            ))
        })
    })
}

/// One entry of `input` for the codec: its bytes, or the ABI code for why not.
fn read_entry(input: &dyn PackInput, path: &str, limit: u64) -> Result<Vec<u8>, i64> {
    let path = RelPath::new(path).map_err(|_| UNSAFE_PATH)?;
    match input.read(&path, limit) {
        Ok(bytes) if u64::try_from(bytes.len()).is_ok_and(|size| size <= limit) => Ok(bytes),
        Ok(_) | Err(PackCodecError::Limit(_)) => Err(LIMIT),
        Err(PackCodecError::FormatMismatch) => Err(NOT_FOUND),
        Err(PackCodecError::UnsafePath(_)) => Err(UNSAFE_PATH),
        Err(_) => Err(INPUT_FAILED),
    }
}

fn validate_imports(module: &Module) -> Result<(), PackCodecError> {
    for import in module.imports() {
        let allowed = import.module() == "msbe_input"
            && matches!(import.name(), "container" | "entries" | "read" | "take")
            && matches!(import.ty(), ExternType::Func(_));
        if !allowed {
            return Err(PackCodecError::Codec(format!(
                "WASM codec requests forbidden import {}.{}",
                import.module(),
                import.name()
            )));
        }
    }
    Ok(())
}

fn instantiate(store: &mut Store<HostState>, module: &Module) -> Result<Instance, PackCodecError> {
    let mut linker = Linker::new(module.engine());
    linker
        .func_wrap(
            "msbe_input",
            "container",
            |caller: Caller<'_, HostState>| match caller.data().container {
                ContainerKind::Zip => 0,
                ContainerKind::Directory => 1,
                ContainerKind::File => 2,
            },
        )
        .map_err(runtime_error)?;
    linker
        .func_wrap(
            "msbe_input",
            "entries",
            |mut caller: Caller<'_, HostState>| {
                let entries = caller.data().entries.clone();
                i32::try_from(stage(&mut caller, entries))
                    .unwrap_or(i32::try_from(LIMIT).unwrap_or(i32::MIN))
            },
        )
        .map_err(runtime_error)?;
    linker
        .func_wrap(
            "msbe_input",
            "read",
            |mut caller: Caller<'_, HostState>, pointer: i32, length: i32, limit: i64| {
                let Some(path) = read_memory(&mut caller, pointer, length)
                    .and_then(|bytes| String::from_utf8(bytes).ok())
                else {
                    return UNSAFE_PATH;
                };
                let asked = u64::try_from(limit).unwrap_or(0);
                let allowed = asked.min(caller.data().read_left);
                if caller.data().reads.send((path, allowed)).is_err() {
                    return INPUT_FAILED;
                }
                let Ok(answer) = caller.data().answers.recv() else {
                    return INPUT_FAILED;
                };
                match answer {
                    Ok(bytes) => {
                        let state = caller.data_mut();
                        state.read_left = state
                            .read_left
                            .saturating_sub(u64::try_from(bytes.len()).unwrap_or(u64::MAX));
                        stage(&mut caller, bytes)
                    }
                    Err(LIMIT) if allowed < asked => BUDGET,
                    Err(code) => code,
                }
            },
        )
        .map_err(runtime_error)?;
    linker
        .func_wrap(
            "msbe_input",
            "take",
            |mut caller: Caller<'_, HostState>, pointer: i32, length: i32| {
                let bytes = std::mem::take(&mut caller.data_mut().staged);
                let length = usize::try_from(length).unwrap_or(0);
                if bytes.len() > length
                    || write_caller_memory(&mut caller, pointer, &bytes).is_err()
                {
                    i32::try_from(LIMIT).unwrap_or(i32::MIN)
                } else {
                    i32::try_from(bytes.len()).unwrap_or(i32::MAX)
                }
            },
        )
        .map_err(runtime_error)?;
    linker.instantiate(store, module).map_err(runtime_error)
}

fn call_without_request(
    store: &mut Store<HostState>,
    instance: &Instance,
    name: &str,
) -> Result<Value, PackCodecError> {
    let packed = instance
        .get_typed_func::<(), i64>(&mut *store, name)
        .map_err(runtime_error)?
        .call(&mut *store, ())
        .map_err(runtime_error)?;
    read_response(store, instance, packed)
}

fn call_with_request(
    store: &mut Store<HostState>,
    instance: &Instance,
    name: &str,
    request: &Value,
) -> Result<Value, PackCodecError> {
    let bytes = serde_json::to_vec(request).map_err(runtime_error)?;
    let size = i32::try_from(bytes.len()).map_err(runtime_error)?;
    let pointer = instance
        .get_typed_func::<i32, i32>(&mut *store, "msbe_alloc")
        .map_err(runtime_error)?
        .call(&mut *store, size)
        .map_err(runtime_error)?;
    write_instance_memory(&mut *store, instance, pointer, &bytes)?;
    let packed = instance
        .get_typed_func::<(i32, i32), i64>(&mut *store, name)
        .map_err(runtime_error)?
        .call(&mut *store, (pointer, size))
        .map_err(runtime_error)?;
    read_response(store, instance, packed)
}

fn read_response(
    store: &mut Store<HostState>,
    instance: &Instance,
    packed: i64,
) -> Result<Value, PackCodecError> {
    let bits = u64::from_ne_bytes(packed.to_ne_bytes());
    let pointer = i32::try_from(bits >> 32).map_err(runtime_error)?;
    let length = usize::try_from(bits & u64::from(u32::MAX)).map_err(runtime_error)?;
    let memory = instance
        .get_memory(&mut *store, "memory")
        .ok_or_else(|| PackCodecError::Codec("WASM codec exports no memory".to_owned()))?;
    let mut bytes = vec![0; length];
    memory
        .read(
            store,
            usize::try_from(pointer).map_err(runtime_error)?,
            &mut bytes,
        )
        .map_err(runtime_error)?;
    serde_json::from_slice(&bytes).map_err(|error| PackCodecError::Codec(error.to_string()))
}

/// The record in a response, typed.
fn record<T: DeserializeOwned>(response: Value) -> Result<T, PackCodecError> {
    serde_json::from_value(decode(response)?)
        .map_err(|error| PackCodecError::Codec(error.to_string()))
}

/// The value of an `{"ok": ...}` response, or the typed error of an `{"error": ...}` one.
fn decode(response: Value) -> Result<Value, PackCodecError> {
    let invalid = || PackCodecError::Codec("codec returned an invalid response".to_owned());
    let Value::Object(mut response) = response else {
        return Err(invalid());
    };
    if let Some(value) = response.remove("ok") {
        return Ok(value);
    }
    Err(response
        .remove("error")
        .map_or_else(invalid, |error| codec_failure(&error)))
}

/// The typed error a codec's `{"kind": ...}` failure stands for, so a sandboxed codec fails exactly
/// as a native one would.
fn codec_failure(error: &Value) -> PackCodecError {
    let text = |field: &str| {
        error
            .get(field)
            .and_then(Value::as_str)
            .map(str::to_owned)
            .unwrap_or_default()
    };
    match error.get("kind").and_then(Value::as_str) {
        Some("invalid_options") => PackCodecError::InvalidOptions(text("message")),
        Some("unsupported_direction") => match text("direction").as_str() {
            "import" => PackCodecError::UnsupportedDirection("import"),
            "export" => PackCodecError::UnsupportedDirection("export"),
            _ => PackCodecError::Codec(error.to_string()),
        },
        Some("unsupported_target") => PackCodecError::UnsupportedTarget {
            game: text("game"),
            loader: text("loader"),
        },
        Some("format_mismatch") => PackCodecError::FormatMismatch,
        Some("limit") => PackCodecError::Limit(text("message")),
        Some("unsafe_path") => PackCodecError::UnsafePath(text("message")),
        Some("missing_blob") => text("digest").parse().map_or_else(
            |_| PackCodecError::Codec(error.to_string()),
            PackCodecError::MissingBlob,
        ),
        Some("unreproducible") => PackCodecError::Unreproducible(text("message")),
        Some("distribution_forbidden") => PackCodecError::DistributionForbidden(text("message")),
        Some("codec") => PackCodecError::Codec(text("message")),
        _ => PackCodecError::Codec(error.to_string()),
    }
}

fn stage(caller: &mut Caller<'_, HostState>, bytes: Vec<u8>) -> i64 {
    let length = i64::try_from(bytes.len()).unwrap_or(BUDGET);
    if length >= 0 {
        caller.data_mut().staged = bytes;
    }
    length
}

fn read_memory(caller: &mut Caller<'_, HostState>, pointer: i32, length: i32) -> Option<Vec<u8>> {
    let memory = caller.get_export("memory")?.into_memory()?;
    let mut bytes = vec![0; usize::try_from(length).ok()?];
    memory
        .read(caller, usize::try_from(pointer).ok()?, &mut bytes)
        .ok()?;
    Some(bytes)
}

fn write_instance_memory(
    store: &mut Store<HostState>,
    instance: &Instance,
    pointer: i32,
    bytes: &[u8],
) -> Result<(), PackCodecError> {
    let memory = instance
        .get_memory(&mut *store, "memory")
        .ok_or_else(|| PackCodecError::Codec("WASM codec exports no memory".to_owned()))?;
    memory
        .write(
            store,
            usize::try_from(pointer).map_err(runtime_error)?,
            bytes,
        )
        .map_err(runtime_error)
}

fn write_caller_memory(
    caller: &mut Caller<'_, HostState>,
    pointer: i32,
    bytes: &[u8],
) -> Result<(), PackCodecError> {
    let memory = caller
        .get_export("memory")
        .and_then(wasmtime::Extern::into_memory)
        .ok_or_else(|| PackCodecError::Codec("WASM codec exports no memory".to_owned()))?;
    memory
        .write(
            caller,
            usize::try_from(pointer).map_err(runtime_error)?,
            bytes,
        )
        .map_err(runtime_error)
}

fn runtime_error(error: impl std::fmt::Display) -> PackCodecError {
    PackCodecError::Codec(error.to_string())
}

#[cfg(test)]
mod tests {
    use std::{cell::RefCell, collections::BTreeMap};

    use msbe_fsops::RelPath;
    use msbe_provider_api::{ExtensionCapability, HostApiRange, PackEntry, SigningKey};

    use super::*;

    fn module(descriptor: &str, probe: &str) -> Vec<u8> {
        let packed = |pointer: usize, length: usize| -> u64 {
            ((pointer as u64) << 32) | u64::try_from(length).unwrap_or(0)
        };
        let descriptor_length = descriptor.len();
        let probe_length = probe.len();
        let wat_string = |value: &str| value.replace('\\', "\\\\").replace('"', "\\\"");
        let descriptor = wat_string(descriptor);
        let probe = wat_string(probe);
        wat::parse_str(format!(
            r#"(module
                (memory (export "memory") 1)
                (data (i32.const 0) "{descriptor}")
                (data (i32.const 1024) "{probe}")
                (func (export "msbe_abi_version") (result i32) i32.const 1)
                (func (export "msbe_descriptor") (result i64) i64.const {})
                (func (export "msbe_probe") (result i64) i64.const {})
            )"#,
            packed(0, descriptor_length),
            packed(1024, probe_length),
        ))
        .unwrap_or_default()
    }

    fn descriptor() -> String {
        r#"{"ok":{"id":"fixture","provider":null,"name":"Fixture","extensions":["fixture"],"media_types":[],"directions":{"import":true,"export":false},"supported_games":{"kind":"universal"},"option_schema":{"schema":1}}}"#.to_owned()
    }

    /// A pack that records every entry read from it.
    #[derive(Debug)]
    struct FixtureInput {
        entries: Vec<PackEntry>,
        files: BTreeMap<RelPath, Vec<u8>>,
        read: RefCell<Vec<String>>,
    }

    impl PackInput for FixtureInput {
        fn container(&self) -> ContainerKind {
            ContainerKind::Zip
        }

        fn entries(&self) -> &[PackEntry] {
            &self.entries
        }

        fn read(&self, path: &RelPath, limit: u64) -> Result<Vec<u8>, PackCodecError> {
            self.read.borrow_mut().push(path.to_string());
            let bytes = self.files.get(path).ok_or(PackCodecError::FormatMismatch)?;
            if u64::try_from(bytes.len()).unwrap_or(u64::MAX) > limit {
                return Err(PackCodecError::Limit(path.to_string()));
            }
            Ok(bytes.clone())
        }
    }

    #[test]
    fn loads_and_calls_a_closed_abi_module() -> Result<(), PackCodecError> {
        let codec = WasmPackCodec::load(&module(
            &descriptor(),
            r#"{"ok":{"confidence":100,"reason":"fixture"}}"#,
        ))?;
        assert_eq!(codec.descriptor().id, "fixture");
        let probe = codec.probe(&EmptyInput)?;
        assert_eq!(probe.confidence, 100);
        Ok(())
    }

    #[test]
    fn codec_failures_keep_their_kind() -> Result<(), PackCodecError> {
        let failing = |error: &str| {
            WasmPackCodec::load(&module(&descriptor(), &format!(r#"{{"error":{error}}}"#)))
        };
        let unreproducible = failing(r#"{"kind":"unreproducible","message":"no index"}"#)?;
        assert!(matches!(
            unreproducible.probe(&EmptyInput),
            Err(PackCodecError::Unreproducible(message)) if message == "no index"
        ));
        let target = failing(r#"{"kind":"unsupported_target","game":"g","loader":"l"}"#)?;
        assert!(matches!(
            target.probe(&EmptyInput),
            Err(PackCodecError::UnsupportedTarget { game, loader }) if game == "g" && loader == "l"
        ));
        let unknown = failing(r#"{"kind":"surprising"}"#)?;
        assert!(matches!(
            unknown.probe(&EmptyInput),
            Err(PackCodecError::Codec(message)) if message.contains("surprising")
        ));
        Ok(())
    }

    #[test]
    fn a_codec_reads_only_the_entries_it_asks_for() -> Result<(), PackCodecError> {
        let path = |raw: &str| RelPath::new(raw).map_err(runtime_error);
        let manifest = br#"{"format":"pack-list","version":1,"files":[],"bundled":[]}"#.to_vec();
        let input = FixtureInput {
            entries: vec![
                PackEntry {
                    path: path("bundled/world.zip")?,
                    size: 1 << 30,
                },
                PackEntry {
                    path: path("pack-list.json")?,
                    size: manifest.len() as u64,
                },
            ],
            files: BTreeMap::from([(path("pack-list.json")?, manifest)]),
            read: RefCell::new(Vec::new()),
        };
        let codec = WasmPackCodec::load(include_bytes!("../tests/fixtures/pack-list.wasm"))?;
        assert_eq!(codec.descriptor().id, "pack-list");
        assert_eq!(codec.probe(&input)?.confidence, 100);
        assert_eq!(input.read.borrow().as_slice(), ["pack-list.json"]);
        Ok(())
    }

    #[test]
    fn rejects_an_import_outside_msbe_input() {
        let module =
            wat::parse_str(r#"(module (import "wasi_snapshot_preview1" "random_get" (func)))"#)
                .unwrap_or_default();
        let error = WasmPackCodec::load(&module).unwrap_err();
        assert!(error.to_string().contains("forbidden import"));
    }

    #[test]
    fn signed_codecs_require_an_empty_capability_grant() -> Result<(), PackCodecError> {
        let payload = module(&descriptor(), r#"{"ok":{"confidence":0,"reason":null}}"#);
        let key = SigningKey::from_bytes(&[7; 32]);
        let mut envelope = ExtensionEnvelope {
            schema: 1,
            package_digest: ExtensionEnvelope::package_digest_for(&payload)
                .map_err(runtime_error)?,
            id: "fixture-codec".to_owned(),
            version: "1.0.0".to_owned(),
            provides: vec![ExtensionProvide::PackCodecV1],
            host_api: HostApiRange {
                minimum: 1,
                maximum: 1,
            },
            capabilities: vec![ExtensionCapability::Network],
            signer: "test".to_owned(),
            signature: "00".repeat(64),
            payload,
        };
        envelope.sign(&key).map_err(runtime_error)?;
        let trusted = BTreeMap::from([("test".to_owned(), key.verifying_key())]);
        let error = WasmPackCodec::load_signed(&envelope, &trusted).unwrap_err();
        assert!(error.to_string().contains("may not request capabilities"));
        Ok(())
    }
}
