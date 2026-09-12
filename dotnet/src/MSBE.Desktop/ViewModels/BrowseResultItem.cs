using System.Diagnostics.CodeAnalysis;

using CommunityToolkit.Mvvm.ComponentModel;

namespace MSBE.Desktop.ViewModels;

/// <summary>A provider-neutral mod search result.</summary>
[SuppressMessage("Design", "CA1515:Consider making public types internal", Justification = "Compiled AXAML item templates reference this type directly.")]
public sealed partial class BrowseResultItem : ObservableObject
{
    /// <summary>Initializes a new instance of the <see cref="BrowseResultItem" /> class.</summary>
    /// <param name="provider">The provider identifier.</param>
    /// <param name="project">The provider's stable project identity.</param>
    /// <param name="reference">The provider-specific project reference.</param>
    /// <param name="title">The display title.</param>
    /// <param name="description">The provider-supplied summary.</param>
    /// <param name="iconSource">The optional provider-supplied HTTPS icon source.</param>
    /// <param name="downloads">The provider-reported download count.</param>
    /// <param name="isInstalled">Whether the selected profile already contains the project.</param>
    public BrowseResultItem(string provider, string project, string reference, string title, string description, string? iconSource, ulong downloads, bool isInstalled)
    {
        this.Provider = provider;
        this.Project = project;
        this.Reference = reference;
        this.Title = title;
        this.Description = description;
        this.IconSource = iconSource;
        this.Downloads = downloads;
        this.IsInstalled = isInstalled;
        this.IsMarked = isInstalled;
    }

    /// <summary>Gets the provider identifier.</summary>
    public string Provider { get; }

    /// <summary>Gets the provider's stable project identity.</summary>
    public string Project { get; }

    /// <summary>Gets the provider-specific project reference.</summary>
    public string Reference { get; }

    /// <summary>Gets the display title.</summary>
    public string Title { get; }

    /// <summary>Gets the provider-supplied summary.</summary>
    public string Description { get; }

    /// <summary>Gets the optional provider-supplied HTTPS icon source.</summary>
    public string? IconSource { get; }

    /// <summary>Gets the provider-reported download count.</summary>
    public ulong Downloads { get; }

    /// <summary>Gets a value indicating whether the selected profile already contains the project.</summary>
    public bool IsInstalled { get; }

    /// <summary>Gets or sets whether this result is marked for installation.</summary>
    [ObservableProperty]
    public partial bool IsMarked { get; set; }

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
