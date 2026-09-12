//! Sandboxed WebAssembly runtime for MSBE pack codecs.
//!
//! The runtime links only the bounded `msbe_input` interface. A module requesting filesystem,
//! network, clock, randomness, or any other host import is rejected before it can run.

#![forbid(unsafe_code)]

use std::collections::BTreeMap;

use msbe_provider_api::{
    ContainerKind, ExtensionEnvelope, ExtensionProvide, PackCodec, PackCodecDescriptor,
    PackCodecError, PackExportContext, PackExportPlan, PackImportContext, PackImportPlan,
    PackInput, PackLayout, PackOptions, PackProbe, VerifyingKey,
};
use serde_json::{Value, json};
use wasmtime::{Caller, Config, Engine, ExternType, Linker, Module, Store};

const ABI_VERSION: i32 = 1;
const HOST_API_VERSION: u32 = 1;
const FUEL: u64 = 1_000_000;
const NOT_FOUND: i64 = -1;
const LIMIT: i64 = -2;
const UNSAFE_PATH: i64 = -3;
const BUDGET: i64 = -4;

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
        let mut config = Config::new();
        config.consume_fuel(true);
        let engine = Engine::new(&config).map_err(runtime_error)?;
        let module = Module::new(&engine, bytes).map_err(runtime_error)?;
        validate_imports(&module)?;
        let mut store = Store::new(&engine, HostState::new(Box::new(EmptyInput)));
        store.set_fuel(FUEL).map_err(runtime_error)?;
        let instance = instantiate(&mut store, &module)?;
        let version = instance
            .get_typed_func::<(), i32>(&mut store, "msbe_abi_version")
            .map_err(runtime_error)?
            .call(&mut store, ())
            .map_err(runtime_error)?;
        if version != ABI_VERSION {
            return Err(PackCodecError::Codec(format!(
                "unsupported WASM codec ABI version {version}"
            )));
        }
        let response = call_without_request(&mut store, &instance, "msbe_descriptor")?;
        let descriptor: PackCodecDescriptor = serde_json::from_value(decode(response)?)
            .map_err(|error| PackCodecError::Codec(error.to_string()))?;
        descriptor.validate()?;
        Ok(Self {
            engine,
            module,
            descriptor,
        })
    }

    fn call_input<T: serde::de::DeserializeOwned>(
        &self,
        input: &dyn PackInput,
        function: &str,
        request: Option<Value>,
    ) -> Result<T, PackCodecError> {
        let input = SnapshotInput::from_input(input)?;
        let mut store = Store::new(&self.engine, HostState::new(Box::new(input)));
        store.set_fuel(FUEL).map_err(runtime_error)?;
        let instance = instantiate(&mut store, &self.module)?;
        let response = match request {
            Some(request) => call_with_request(&mut store, &instance, function, &request)?,
            None => call_without_request(&mut store, &instance, function)?,
        };
        serde_json::from_value(decode(response)?)
            .map_err(|error| PackCodecError::Codec(error.to_string()))
    }

    fn call<T: serde::de::DeserializeOwned>(
        &self,
        function: &str,
        request: &Value,
    ) -> Result<T, PackCodecError> {
        let mut store = Store::new(&self.engine, HostState::new(Box::new(EmptyInput)));
        store.set_fuel(FUEL).map_err(runtime_error)?;
        let instance = instantiate(&mut store, &self.module)?;
        let response = call_with_request(&mut store, &instance, function, request)?;
        serde_json::from_value(decode(response)?)
            .map_err(|error| PackCodecError::Codec(error.to_string()))
    }
}

impl PackCodec for WasmPackCodec {
    fn descriptor(&self) -> &PackCodecDescriptor {
        &self.descriptor
    }

    fn probe(&self, input: &dyn PackInput) -> Result<PackProbe, PackCodecError> {
        self.call_input(input, "msbe_probe", None)
    }

    fn plan_import(
        &self,
        input: &dyn PackInput,
        context: &PackImportContext,
        options: &PackOptions,
    ) -> Result<PackImportPlan, PackCodecError> {
        self.call_input(
            input,
            "msbe_plan_import",
            Some(json!({ "context": context, "options": options })),
        )
    }

    fn plan_export(
        &self,
        context: &PackExportContext<'_>,
        options: &PackOptions,
    ) -> Result<PackExportPlan, PackCodecError> {
        self.call(
            "msbe_plan_export",
            &json!({
                "context": {
                    "game": context.game,
                    "target": context.target,
                    "lockfile": context.lockfile,
                    "files": context.files,
                    "observations": context.observations,
                    "inclusion": context.inclusion,
                },
                "options": options,
            }),
        )
    }

    fn layout(&self, plan: &PackExportPlan) -> Result<PackLayout, PackCodecError> {
        self.call(
            "msbe_layout",
            &serde_json::to_value(plan).map_err(runtime_error)?,
        )
    }
}

#[derive(Debug)]
struct HostState {
    input: Box<dyn HostInput>,
    staged: Vec<u8>,
}

impl HostState {
    fn new(input: Box<dyn HostInput>) -> Self {
        Self {
            input,
            staged: Vec::new(),
        }
    }
}

trait HostInput: std::fmt::Debug + Send + Sync {
    fn container(&self) -> ContainerKind;
    fn entries(&self) -> Vec<msbe_provider_api::PackEntry>;
    fn read(&self, path: &str, limit: u64) -> Result<Vec<u8>, i64>;
}

#[derive(Debug)]
struct SnapshotInput {
    container: ContainerKind,
    entries: Vec<msbe_provider_api::PackEntry>,
    files: BTreeMap<String, Vec<u8>>,
}

impl SnapshotInput {
    fn from_input(input: &dyn PackInput) -> Result<Self, PackCodecError> {
        let entries = input.entries().to_vec();
        let mut files = BTreeMap::new();
        for entry in &entries {
            let bytes = input
                .read(&entry.path, entry.size)
                .map_err(|error| PackCodecError::Codec(error.to_string()))?;
            files.insert(entry.path.to_string(), bytes);
        }
        Ok(Self {
            container: input.container(),
            entries,
            files,
        })
    }
}

impl HostInput for SnapshotInput {
    fn container(&self) -> ContainerKind {
        self.container
    }

    fn entries(&self) -> Vec<msbe_provider_api::PackEntry> {
        self.entries.clone()
    }

    fn read(&self, path: &str, limit: u64) -> Result<Vec<u8>, i64> {
        let bytes = self.files.get(path).ok_or(NOT_FOUND)?;
        if u64::try_from(bytes.len()).unwrap_or(u64::MAX) > limit {
            return Err(LIMIT);
        }
        Ok(bytes.clone())
    }
}

#[derive(Debug)]
struct EmptyInput;

impl HostInput for EmptyInput {
    fn container(&self) -> ContainerKind {
        ContainerKind::File
    }

    fn entries(&self) -> Vec<msbe_provider_api::PackEntry> {
        Vec::new()
    }

    fn read(&self, _: &str, _: u64) -> Result<Vec<u8>, i64> {
        Err(NOT_FOUND)
    }
}

impl PackInput for EmptyInput {
    fn container(&self) -> ContainerKind {
        ContainerKind::File
    }

    fn entries(&self) -> &[msbe_provider_api::PackEntry] {
        &[]
    }

    fn read(&self, _: &msbe_fsops::RelPath, _: u64) -> Result<Vec<u8>, PackCodecError> {
        Err(PackCodecError::Codec("entry not found".to_owned()))
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

fn instantiate(
    store: &mut Store<HostState>,
    module: &Module,
) -> Result<wasmtime::Instance, PackCodecError> {
    let mut linker = Linker::new(module.engine());
    linker
        .func_wrap(
            "msbe_input",
            "container",
            |caller: Caller<'_, HostState>| match caller.data().input.container() {
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
                let entries = caller.data().input.entries();
                match serde_json::to_vec(&entries) {
                    Ok(bytes) => i32::try_from(stage(&mut caller, bytes))
                        .unwrap_or(i32::try_from(LIMIT).unwrap_or(i32::MIN)),
                    Err(_) => i32::try_from(LIMIT).unwrap_or(i32::MIN),
                }
            },
        )
        .map_err(runtime_error)?;
    linker
        .func_wrap(
            "msbe_input",
            "read",
            |mut caller: Caller<'_, HostState>, pointer: i32, length: i32, limit: i64| {
                let Some(path) = read_memory(&mut caller, pointer, length) else {
                    return UNSAFE_PATH;
                };
                let Ok(path) = std::str::from_utf8(&path) else {
                    return UNSAFE_PATH;
                };
                let limit = u64::try_from(limit).unwrap_or(0);
                match caller.data().input.read(path, limit) {
                    Ok(bytes) => stage(&mut caller, bytes),
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
    instance: &wasmtime::Instance,
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
    instance: &wasmtime::Instance,
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
    instance: &wasmtime::Instance,
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

fn decode(response: Value) -> Result<Value, PackCodecError> {
    let Value::Object(mut response) = response else {
        return Err(PackCodecError::Codec(
            "codec returned an invalid response".to_owned(),
        ));
    };
    if let Some(value) = response.remove("ok") {
        return Ok(value);
    }
    Err(PackCodecError::Codec(response.get("error").map_or_else(
        || "codec returned an invalid response".to_owned(),
        Value::to_string,
    )))
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
    instance: &wasmtime::Instance,
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
    use std::collections::BTreeMap;

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

    #[derive(Debug)]
    struct FixtureInput {
        entries: Vec<PackEntry>,
        files: BTreeMap<RelPath, Vec<u8>>,
    }

    impl PackInput for FixtureInput {
        fn container(&self) -> ContainerKind {
            ContainerKind::Zip
        }

        fn entries(&self) -> &[PackEntry] {
            &self.entries
        }

        fn read(&self, path: &RelPath, limit: u64) -> Result<Vec<u8>, PackCodecError> {
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
    fn probes_the_third_party_pack_list_codec() -> Result<(), PackCodecError> {
        let path = RelPath::new("pack-list.json")
            .map_err(|error| PackCodecError::Codec(error.to_string()))?;
        let bytes = br#"{"format":"pack-list","version":1,"files":[],"bundled":[]}"#.to_vec();
        let input = FixtureInput {
            entries: vec![PackEntry {
                path: path.clone(),
                size: bytes.len() as u64,
            }],
            files: BTreeMap::from([(path, bytes)]),
        };
        let codec = WasmPackCodec::load(include_bytes!("../tests/fixtures/pack-list.wasm"))?;
        assert_eq!(codec.descriptor().id, "pack-list");
        assert_eq!(codec.probe(&input)?.confidence, 100);
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
