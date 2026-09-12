namespace MSBE.Client;

/// <summary>Player-facing information about a game supported by the daemon.</summary>
/// <param name="Id">The stable game identifier.</param>
/// <param name="Name">The display name.</param>
/// <param name="SupportVersion">The loaded support-definition version.</param>
/// <param name="Loaders">The supported mod loading ecosystems.</param>
public sealed record GameInfo(string Id, string Name, string SupportVersion, IReadOnlyList<string> Loaders);
