namespace MSBE.Client;

/// <summary>One mod's claim on a conflicting path.</summary>
/// <param name="Module">The mod.</param>
/// <param name="Blob">The digest of the contents it would place.</param>
public sealed record ConflictClaimInfo(string Module, string Blob);
