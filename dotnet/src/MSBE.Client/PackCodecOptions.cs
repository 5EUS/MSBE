using System.Text.Json;

namespace MSBE.Client;

/// <summary>A codec option schema and its normalized values.</summary>
/// <param name="Codec">The codec ID.</param>
/// <param name="Name">The codec display name.</param>
/// <param name="Fields">Every field, common policy fields first.</param>
/// <param name="Presets">Available presets.</param>
/// <param name="Values">Every field value as plain JSON, keyed by option key.</param>
public sealed record PackCodecOptions(string Codec, string Name, IReadOnlyList<PackOptionField> Fields, IReadOnlyList<PackPreset> Presets, JsonElement Values);
