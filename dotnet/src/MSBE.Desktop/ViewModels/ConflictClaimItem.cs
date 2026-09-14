using System.Diagnostics.CodeAnalysis;

using MSBE.Client;
using MSBE.Desktop.Resources;

namespace MSBE.Desktop.ViewModels;

/// <summary>One mod's claim on a conflicting path, with the action that resolves it.</summary>
[SuppressMessage("Design", "CA1515:Consider making public types internal", Justification = "Compiled AXAML item templates reference this type directly.")]
public sealed class ConflictClaimItem
{
    private const int ShownDigestLength = 19;

    /// <summary>Initializes a new instance of the <see cref="ConflictClaimItem" /> class.</summary>
    /// <param name="claim">The claim, as the daemon reported it.</param>
    internal ConflictClaimItem(ConflictClaimInfo claim)
    {
        this.Module = claim.Module;
        this.Detail = Strings.FormatConflictClaim(claim.Blob.Length > ShownDigestLength ? claim.Blob[..ShownDigestLength] : claim.Blob);
        this.RemoveText = Strings.FormatConflictRemoveMod(claim.Module);
    }

    /// <summary>Gets the mod.</summary>
    public string Module { get; }

    /// <summary>Gets the start of the digest of what the mod would place.</summary>
    public string Detail { get; }

    /// <summary>Gets the label of the action that removes the mod from the profile.</summary>
    public string RemoveText { get; }
}
