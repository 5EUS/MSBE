namespace MSBE.Client;

/// <summary>A path that more than one mod in a profile would place, with different contents.</summary>
/// <param name="Path">The path, relative to the instance.</param>
/// <param name="Claims">Every mod that places it, and what it would place.</param>
public sealed record ConflictInfo(string Path, IReadOnlyList<ConflictClaimInfo> Claims);
