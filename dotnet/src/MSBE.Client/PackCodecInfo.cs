namespace MSBE.Client;

/// <summary>A pack codec the daemon discovered from its reviewed registry.</summary>
/// <param name="Id">The stable codec ID.</param>
/// <param name="Name">The display name.</param>
/// <param name="Extensions">Recognized file extensions without a leading dot.</param>
/// <param name="CanImport">Whether the codec imports.</param>
/// <param name="CanExport">Whether the codec exports.</param>
/// <param name="IsProviderNeutral">Whether the format belongs to no provider, as MSBE's own bundle does.</param>
public sealed record PackCodecInfo(string Id, string Name, IReadOnlyList<string> Extensions, bool CanImport, bool CanExport, bool IsProviderNeutral);
