namespace MSBE.Client;

/// <summary>What updating a profile's mods from their providers found, or did.</summary>
/// <param name="IsDryRun">Whether nothing was changed.</param>
/// <param name="Updated">The mods with a newer compatible release.</param>
/// <param name="Current">Mods already on their newest compatible release.</param>
/// <param name="NoCompatibleVersion">Mods whose provider has no release for the profile's target.</param>
/// <param name="Unlisted">Mods their provider no longer lists.</param>
/// <param name="NotUpdatable">Mods no registered provider can update, such as local files.</param>
/// <param name="Unresolved">Requirements no release could meet.</param>
/// <param name="Incompatible">Declared incompatibilities between mods in the profile.</param>
public sealed record UpdateReportInfo(
    bool IsDryRun,
    IReadOnlyList<ModUpdateInfo> Updated,
    IReadOnlyList<string> Current,
    IReadOnlyList<string> NoCompatibleVersion,
    IReadOnlyList<string> Unlisted,
    IReadOnlyList<string> NotUpdatable,
    IReadOnlyList<RequirementInfo> Unresolved,
    IReadOnlyList<RequirementInfo> Incompatible);
