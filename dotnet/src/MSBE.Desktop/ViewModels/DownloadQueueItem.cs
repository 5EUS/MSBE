using System.Diagnostics.CodeAnalysis;

using CommunityToolkit.Mvvm.ComponentModel;

using MSBE.Client;

namespace MSBE.Desktop.ViewModels;

/// <summary>One item in the daemon's download queue, as the Downloads page shows it.</summary>
[SuppressMessage("Design", "CA1515:Consider making public types internal", Justification = "Compiled AXAML item templates reference this type directly.")]
public sealed partial class DownloadQueueItem : ObservableObject
{
    /// <summary>Initializes a new instance of the <see cref="DownloadQueueItem" /> class.</summary>
    /// <param name="download">The item the daemon reported.</param>
    /// <param name="iconSource">The optional provider-supplied HTTPS icon source.</param>
    internal DownloadQueueItem(DownloadInfo download, string? iconSource)
    {
        this.Id = download.Id;
        this.IconSource = iconSource;
        this.Update(download);
    }

    /// <summary>Gets the daemon's identifier for the download.</summary>
    public long Id { get; }

    /// <summary>Gets the optional provider-supplied HTTPS icon source.</summary>
    public string? IconSource { get; }

    /// <summary>Gets or sets the display title.</summary>
    [ObservableProperty]
    public partial string Title { get; set; } = string.Empty;

    /// <summary>Gets or sets where the download comes from and where it goes.</summary>
    [ObservableProperty]
    public partial string Summary { get; set; } = string.Empty;

    /// <summary>Gets or sets the instance the download is added to, until one is chosen <see langword="null" />.</summary>
    [ObservableProperty]
    [NotifyPropertyChangedFor(nameof(NeedsProfile))]
    [NotifyPropertyChangedFor(nameof(StatusText))]
    public partial string? Instance { get; set; }

    /// <summary>Gets or sets the profile the download is added to, until one is chosen <see langword="null" />.</summary>
    [ObservableProperty]
    public partial string? Profile { get; set; }

    /// <summary>Gets or sets where the download is in the queue's lifecycle.</summary>
    [ObservableProperty]
    [NotifyPropertyChangedFor(nameof(IsCompleted))]
    [NotifyPropertyChangedFor(nameof(IsFailed))]
    [NotifyPropertyChangedFor(nameof(IsFinished))]
    [NotifyPropertyChangedFor(nameof(IsActive))]
    [NotifyPropertyChangedFor(nameof(IsAwaitingUser))]
    [NotifyPropertyChangedFor(nameof(NeedsProfile))]
    [NotifyPropertyChangedFor(nameof(StatusText))]
    public partial DownloadState State { get; set; }

    /// <summary>Gets or sets what the download is waiting for, what it added, or why it failed.</summary>
    [ObservableProperty]
    [NotifyPropertyChangedFor(nameof(HasDetail))]
    public partial string Detail { get; set; } = string.Empty;

    /// <summary>Gets a value indicating whether the download was added to its profile.</summary>
    public bool IsCompleted => this.State == DownloadState.Completed;

    /// <summary>Gets a value indicating whether the download failed or was cancelled, and can be retried.</summary>
    public bool IsFailed => this.State is DownloadState.Failed or DownloadState.Cancelled;

    /// <summary>Gets a value indicating whether nothing more happens to the download unless it is retried.</summary>
    public bool IsFinished => this.IsCompleted || this.IsFailed;

    /// <summary>Gets a value indicating whether the daemon is working on the download.</summary>
    public bool IsActive => this.State is DownloadState.Resolving or DownloadState.Downloading or DownloadState.Adding;

    /// <summary>Gets a value indicating whether the download waits for the user to start it on a provider page.</summary>
    public bool IsAwaitingUser => this.State == DownloadState.AwaitingUser;

    /// <summary>Gets a value indicating whether a link started the download and it needs a profile.</summary>
    public bool NeedsProfile => this.State == DownloadState.Downloaded && this.Instance is null;

    /// <summary>Gets a value indicating whether there is detail to show.</summary>
    public bool HasDetail => this.Detail.Length > 0;

    /// <summary>Gets a short account of the state.</summary>
    public string StatusText => this.State switch
    {
        DownloadState.Queued => "Queued",
        DownloadState.Paused => "Paused",
        DownloadState.Resolving => "Resolving",
        DownloadState.Downloading => "Downloading",
        DownloadState.AwaitingUser => "Waiting for you",
        DownloadState.Downloaded when this.Instance is null => "Choose a profile",
        DownloadState.Downloaded => "Downloaded",
        DownloadState.Adding => "Adding",
        DownloadState.Completed => "Added",
        DownloadState.Failed => "Failed",
        _ => "Cancelled",
    };

    /// <summary>Shows what the daemon now reports about the download.</summary>
    /// <param name="download">The item the daemon reported.</param>
    internal void Update(DownloadInfo download)
    {
        DownloadFileInfo? first = download.Files.Count > 0 ? download.Files[0] : null;
        this.Title = download.Title ?? download.Source ?? (first is { Name.Length: > 0 } ? first.Name : $"Download {download.Id}");
        string provider = first?.Provider ?? ProviderOf(download.Source);
        this.Instance = download.Instance;
        this.Profile = download.Profile;
        this.Summary = download.Instance is null
            ? $"{provider} · no profile yet"
            : $"{provider} · {download.Instance} / {download.Profile}";
        this.State = download.State switch
        {
            "queued" => DownloadState.Queued,
            "paused" => DownloadState.Paused,
            "resolving" => DownloadState.Resolving,
            "downloading" => DownloadState.Downloading,
            "awaiting_user" => DownloadState.AwaitingUser,
            "downloaded" => DownloadState.Downloaded,
            "adding" => DownloadState.Adding,
            "completed" => DownloadState.Completed,
            "cancelled" => DownloadState.Cancelled,
            _ => DownloadState.Failed,
        };
        this.Detail = Describe(download);
    }

    private static string ProviderOf(string? source)
    {
        if (source is null)
        {
            return "link";
        }

        int colon = source.IndexOf(':', StringComparison.Ordinal);
        return source.StartsWith("https://", StringComparison.OrdinalIgnoreCase) || colon <= 0 ? "url" : source[..colon];
    }

    private static string Describe(DownloadInfo download)
    {
        int downloaded = download.Files.Count(file => string.Equals(file.State, "downloaded", StringComparison.Ordinal));
        string progress = download.Files.Count > 1 ? $"{downloaded} of {download.Files.Count} files downloaded" : string.Empty;
        string warnings = download.Warnings.Count == 0 ? string.Empty : $" · {download.Warnings.Count} unresolved";
        string added = (download.Added.Count, download.Skipped.Count) switch
        {
            (0, > 0) => "Already in profile",
            (0, _) => "Nothing was added",
            (1, _) => "Added 1 mod",
            (int count, _) => $"Added {count} mods",
        };
        return download.State switch
        {
            "awaiting_user" => $"Start the download at {download.Page}",
            "failed" => download.Message ?? "The download failed.",
            "cancelled" => "Cancelled",
            "downloaded" when download.Instance is null => "A link started this download. Add it to a profile.",
            "completed" => added + warnings,
            _ => progress,
        };
    }
}
