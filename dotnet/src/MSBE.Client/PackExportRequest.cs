using System.Text.Json;

namespace MSBE.Client;

/// <summary>What to export.</summary>
/// <param name="Instance">The instance.</param>
/// <param name="Profile">The profile.</param>
/// <param name="Codec">The codec ID.</param>
/// <param name="Preset">The preset to start from, if any.</param>
/// <param name="Options">Option values as plain JSON, applied over the preset.</param>
/// <param name="Output">The absolute destination path.</param>
public sealed record PackExportRequest(string Instance, string Profile, string Codec, string? Preset, IReadOnlyDictionary<string, JsonElement> Options, string Output);
