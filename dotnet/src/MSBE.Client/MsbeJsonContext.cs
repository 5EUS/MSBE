using System.Text.Json.Serialization;

namespace MSBE.Client;

/// <summary>
/// Source-generated serialization for every RPC payload.
/// </summary>
/// <remarks>
/// Reflection-based <c>System.Text.Json</c> is banned in <c>BannedSymbols.txt</c>: it
/// compiles cleanly and then throws on a published NativeAOT build. Types reach this
/// context by being generated from <c>msbe-rpc-schema</c>, so the contract and the
/// serializer cannot drift apart.
/// </remarks>
[JsonSourceGenerationOptions(PropertyNamingPolicy = JsonKnownNamingPolicy.SnakeCaseLower)]
[JsonSerializable(typeof(DaemonInfo))]
[JsonSerializable(typeof(GameInfo[]))]
public sealed partial class MsbeJsonContext : JsonSerializerContext;
