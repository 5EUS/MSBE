using System.Diagnostics.CodeAnalysis;

using MSBE.Client;
using MSBE.Desktop.Resources;

namespace MSBE.Desktop.ViewModels;

/// <summary>A path more than one mod would place, as the conflict tree shows it.</summary>
[SuppressMessage("Design", "CA1515:Consider making public types internal", Justification = "Compiled AXAML item templates reference this type directly.")]
public sealed class ConflictItem
{
    /// <summary>Initializes a new instance of the <see cref="ConflictItem" /> class.</summary>
    /// <param name="conflict">The conflict, as the daemon reported it.</param>
    internal ConflictItem(ConflictInfo conflict)
    {
        this.Path = conflict.Path;
        this.Claims = [.. conflict.Claims.Select(claim => new ConflictClaimItem(claim))];
        this.Summary = Strings.FormatConflictClaims(conflict.Claims.Count);
    }

    /// <summary>Gets the path, relative to the instance.</summary>
    public string Path { get; }

    /// <summary>Gets every mod that places the path.</summary>
    public IReadOnlyList<ConflictClaimItem> Claims { get; }

    /// <summary>Gets how many mods place the path.</summary>
    public string Summary { get; }
}
