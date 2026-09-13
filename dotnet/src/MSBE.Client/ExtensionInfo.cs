namespace MSBE.Client;

/// <summary>An extension installed in the daemon's data directory, and whether it runs.</summary>
/// <param name="Kind"><c>trust_root</c>, <c>codec</c> or <c>program</c>.</param>
/// <param name="Path">The envelope document, or the trust root or folder that could not be read.</param>
/// <param name="Id">The extension ID, when its envelope could be read.</param>
/// <param name="Version">The extension version, when its envelope could be read.</param>
/// <param name="Signer">The signer its envelope names, when it could be read.</param>
/// <param name="Digest">The package digest its envelope declares, when it could be read.</param>
/// <param name="IsActive">Whether it was admitted and runs.</param>
/// <param name="Reason">Why it was refused, when it was.</param>
public sealed record ExtensionInfo(string Kind, string Path, string? Id, string? Version, string? Signer, string? Digest, bool IsActive, string? Reason);
