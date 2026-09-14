namespace MSBE.Client;

/// <summary>A mod that has a newer compatible release.</summary>
/// <param name="Module">The mod.</param>
/// <param name="From">The release in the profile.</param>
/// <param name="To">The release it updates to.</param>
public sealed record ModUpdateInfo(string Module, string From, string To);
