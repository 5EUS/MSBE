using System.Diagnostics.CodeAnalysis;

using CommunityToolkit.Mvvm.ComponentModel;

namespace MSBE.Desktop.ViewModels;

/// <summary>One provider project in the download queue.</summary>
[SuppressMessage("Design", "CA1515:Consider making public types internal", Justification = "Compiled AXAML item templates reference this type directly.")]
public sealed partial class DownloadQueueItem : ObservableObject
{
    /// <summary>Initializes a new instance of the <see cref="DownloadQueueItem" /> class.</summary>
    /// <param name="title">The display title.</param>
    /// <param name="source">The source accepted by the add command.</param>
    /// <param name="provider">The provider identifier.</param>
    /// <param name="iconSource">The optional provider-supplied HTTPS icon source.</param>
    /// <param name="instance">The instance the project is added to.</param>
    /// <param name="profile">The profile the project is added to.</param>
    /// <param name="withDependencies">Whether required provider dependencies are included.</param>
    public DownloadQueueItem(string title, string source, string provider, string? iconSource, string instance, string profile, bool withDependencies)
    {
        this.Title = title;
        this.Source = source;
        this.Provider = provider;
        this.IconSource = iconSource;
        this.Instance = instance;
        this.Profile = profile;
        this.WithDependencies = withDependencies;
    }

    /// <summary>Gets the display title.</summary>
    public string Title { get; }

    /// <summary>Gets the source accepted by the add command.</summary>
    public string Source { get; }

    /// <summary>Gets the provider identifier.</summary>
    public string Provider { get; }

    /// <summary>Gets the optional provider-supplied HTTPS icon source.</summary>
    public string? IconSource { get; }

    /// <summary>Gets the instance the project is added to.</summary>
    public string Instance { get; }

    /// <summary>Gets the profile the project is added to.</summary>
    public string Profile { get; }

    /// <summary>Gets a value indicating whether required provider dependencies are included.</summary>
    public bool WithDependencies { get; }

    /// <summary>Gets where the project comes from and where it goes.</summary>
    public string Summary => $"{this.Provider} · {this.Instance} / {this.Profile}";

    /// <summary>Gets or sets where the download is in the queue's lifecycle.</summary>
    [ObservableProperty]
    [NotifyPropertyChangedFor(nameof(IsCompleted))]
    [NotifyPropertyChangedFor(nameof(IsFailed))]
    public partial DownloadState State { get; set; }

    /// <summary>Gets or sets what the add reported, or why it failed.</summary>
    [ObservableProperty]
    public partial string Detail { get; set; } = string.Empty;

    /// <summary>Gets a value indicating whether the project was added.</summary>
    public bool IsCompleted => this.State == DownloadState.Completed;

    /// <summary>Gets a value indicating whether the download failed.</summary>
    public bool IsFailed => this.State == DownloadState.Failed;

    /// <summary>Determines whether another download adds the same project to the same profile.</summary>
    /// <param name="other">The download to compare with.</param>
    /// <returns><see langword="true" /> if both downloads have the same effect.</returns>
    public bool Targets(DownloadQueueItem other) =>
        string.Equals(this.Instance, other.Instance, StringComparison.Ordinal) &&
        string.Equals(this.Profile, other.Profile, StringComparison.Ordinal) &&
        string.Equals(this.Source, other.Source, StringComparison.Ordinal);
}
