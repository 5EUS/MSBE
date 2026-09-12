namespace MSBE.Client;

/// <summary>A named complete set of option values.</summary>
/// <param name="Id">The stable preset ID.</param>
/// <param name="Name">The display name.</param>
public sealed record PackPreset(string Id, string Name);
