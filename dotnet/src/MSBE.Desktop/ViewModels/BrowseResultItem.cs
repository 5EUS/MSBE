using System.Diagnostics.CodeAnalysis;

namespace MSBE.Desktop.ViewModels;

/// <summary>A provider-neutral mod search result.</summary>
/// <param name="Provider">The provider identifier.</param>
/// <param name="Reference">The provider-specific project reference.</param>
/// <param name="Title">The display title.</param>
/// <param name="Description">The provider-supplied summary.</param>
/// <param name="Downloads">The provider-reported download count.</param>
[SuppressMessage("Design", "CA1515:Consider making public types internal", Justification = "Compiled AXAML item templates reference this type directly.")]
public sealed record BrowseResultItem(string Provider, string Reference, string Title, string Description, ulong Downloads)
{
    /// <summary>Gets the source accepted by the add command.</summary>
    public string Source => $"{this.Provider}:{this.Reference}";

    /// <summary>Gets a compact popularity label.</summary>
    public string DownloadSummary => this.Downloads switch
    {
        >= 1_000_000 => $"{this.Downloads / 1_000_000D:0.#}M downloads",
        >= 1_000 => $"{this.Downloads / 1_000D:0.#}K downloads",
        _ => $"{this.Downloads} downloads",
    };
}
