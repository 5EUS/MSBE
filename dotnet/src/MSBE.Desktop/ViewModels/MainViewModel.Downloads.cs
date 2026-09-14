using System.Collections.ObjectModel;
using System.Collections.Specialized;
using System.Net.Sockets;
using System.Text.Json;

using CommunityToolkit.Mvvm.ComponentModel;
using CommunityToolkit.Mvvm.Input;

using MSBE.Client;

namespace MSBE.Desktop.ViewModels;

/// <content>
/// The Downloads page: a view over the daemon's download queue, which downloads provider projects and
/// adds them to profiles while no window is open.
/// </content>
internal sealed partial class MainViewModel
{
    private static readonly TimeSpan BusyDownloadPollInterval = TimeSpan.FromSeconds(1);
    private static readonly TimeSpan IdleDownloadPollInterval = TimeSpan.FromSeconds(5);

    private readonly Dictionary<long, DownloadQueueItem> downloads = [];
    private readonly Dictionary<long, string> downloadIcons = [];
    private readonly List<string> pendingLinks = [];
    private string? browserMessage;
    private List<long> downloadOrder = [];
    private long downloadRevision;
    private Task? downloadPoller;

    /// <summary>Gets unfinished downloads waiting their turn or waiting for the user, in queue order.</summary>
    public ObservableCollection<DownloadQueueItem> QueuedDownloads { get; } = [];

    /// <summary>Gets finished downloads, newest first.</summary>
    public ObservableCollection<DownloadQueueItem> FinishedDownloads { get; } = [];

    /// <summary>Gets or sets the download the daemon is working on.</summary>
    [ObservableProperty]
    public partial DownloadQueueItem? ActiveDownload { get; set; }

    /// <summary>Gets or sets a value indicating whether the queue holds off starting the next download.</summary>
    [ObservableProperty]
    public partial bool IsDownloadQueuePaused { get; set; }

    /// <summary>Gets or sets a value indicating whether the daemon owns a download queue.</summary>
    [ObservableProperty]
    public partial bool IsDownloadQueueSupported { get; set; }

    /// <summary>Gets or sets a link the user pasted from a provider page, for when no link handler is registered.</summary>
    [ObservableProperty]
    public partial string HandoffLink { get; set; } = string.Empty;

    /// <summary>Gets or sets a value indicating whether the MSBE browser is open.</summary>
    [ObservableProperty]
    [NotifyPropertyChangedFor(nameof(CanOpenWaitingPages))]
    public partial bool IsBrowserOpen { get; set; }

    /// <summary>Gets or sets how many files wait for the user on a provider page.</summary>
    [ObservableProperty]
    [NotifyPropertyChangedFor(nameof(CanOpenWaitingPages))]
    public partial long WaitingPageCount { get; set; }

    /// <summary>Gets or sets a one-line account of what the MSBE browser shows.</summary>
    [ObservableProperty]
    public partial string BrowserSummary { get; set; } = string.Empty;

    /// <summary>Gets or sets a value indicating whether the MSBE browser goes to the next page once a download or link arrives.</summary>
    [ObservableProperty]
    public partial bool IsBrowserAutoAdvancing { get; set; }

    /// <summary>Gets a value indicating whether files wait on a page and the MSBE browser is closed.</summary>
    public bool CanOpenWaitingPages => this.WaitingPageCount > 0 && !this.IsBrowserOpen;

    /// <summary>Gets a value indicating whether the daemon is working on a download.</summary>
    public bool IsDownloading => this.ActiveDownload is not null;

    /// <summary>Gets the number of downloads running or waiting.</summary>
    public int PendingDownloadCount => this.QueuedDownloads.Count + (this.IsDownloading ? 1 : 0);

    /// <summary>Gets a value indicating whether any download is running or waiting.</summary>
    public bool HasPendingDownloads => this.PendingDownloadCount > 0;

    /// <summary>Gets a value indicating whether any download is waiting.</summary>
    public bool HasQueuedDownloads => this.QueuedDownloads.Count > 0;

    /// <summary>Gets a value indicating whether any download has finished.</summary>
    public bool HasFinishedDownloads => this.FinishedDownloads.Count > 0;

    /// <summary>Gets a one-line account of the queue.</summary>
    public string DownloadSummary => (this.ActiveDownload, this.QueuedDownloads.Count) switch
    {
        (null, 0) => "No downloads in progress",
        (null, int queued) when this.IsDownloadQueuePaused => $"Paused · {queued} queued",
        (null, int queued) => $"{queued} queued",
        ({ } active, 0) => $"{active.StatusText} {active.Title}",
        ({ } active, _) when this.IsDownloadQueuePaused => $"{active.StatusText} {active.Title} · queue paused",
        ({ } active, int queued) => $"{active.StatusText} {active.Title} · {queued} queued",
    };

    /// <summary>Gets the label of the pause toggle.</summary>
    public string DownloadQueueToggleLabel => this.IsDownloadQueuePaused ? "Resume queue" : "Pause queue";

    /// <summary>Gets the heading shown while nothing is downloading.</summary>
    public string DownloadIdleTitle => this.IsDownloadQueuePaused && this.HasQueuedDownloads ? "Queue paused" : "Nothing downloading";

    /// <summary>Gets the hint shown while nothing is downloading.</summary>
    public string DownloadIdleHint => this.IsDownloadQueuePaused && this.HasQueuedDownloads
        ? "Resume the queue to start the next download. Links from your browser are still received."
        : "Mods you install from Browse line up here, and MSBE keeps downloading them while this window is closed.";

    /// <summary>Hands a link the operating system opened MSBE with to the daemon's download queue, holding it until the daemon connects.</summary>
    /// <param name="link">The link a provider page handed over.</param>
    /// <returns>A task that completes once the daemon has the link, or once it is held.</returns>
    public Task ReceiveLinkAsync(Uri link)
    {
        ArgumentNullException.ThrowIfNull(link);
        if (!this.IsDownloadQueueSupported)
        {
            this.pendingLinks.Add(link.OriginalString);
            return Task.CompletedTask;
        }

        return this.SubmitLinkAsync(link.OriginalString);
    }

    private static void Replace(ObservableCollection<DownloadQueueItem> target, List<DownloadQueueItem> items)
    {
        if (target.SequenceEqual(items))
        {
            return;
        }

        target.Clear();
        foreach (DownloadQueueItem item in items)
        {
            target.Add(item);
        }
    }

    partial void OnActiveDownloadChanged(DownloadQueueItem? value) => this.NotifyDownloadsChanged();

    partial void OnIsDownloadQueuePausedChanged(bool value) => this.NotifyDownloadsChanged();

    /// <summary>Follows the daemon's download queue, often while downloads are pending and seldom otherwise.</summary>
    private async Task PollDownloadsAsync()
    {
        while (this.IsDownloadQueueSupported)
        {
            await this.RefreshDownloadsAsync().ConfigureAwait(true);
            await Task.Delay(this.HasPendingDownloads ? BusyDownloadPollInterval : IdleDownloadPollInterval, this.time).ConfigureAwait(true);
        }

        this.downloadPoller = null;
    }

    /// <summary>Starts following the daemon's download queue, unless it already is.</summary>
    private void StartDownloadPolling() => this.downloadPoller ??= this.PollDownloadsAsync();

    [RelayCommand]
    private async Task RefreshDownloadsAsync()
    {
        if (!this.IsDownloadQueueSupported)
        {
            return;
        }

        try
        {
            DownloadListInfo list = await this.client.ListDownloadsAsync(this.downloadRevision, CancellationToken.None).ConfigureAwait(true);
            await this.ApplyDownloadsAsync(list).ConfigureAwait(true);
        }
        catch (Exception exception) when (exception is IOException or SocketException or JsonException or InvalidOperationException or KeyNotFoundException)
        {
            this.StatusMessage = $"Could not read the download queue: {exception.Message}";
            return;
        }

        try
        {
            this.ApplyBrowserStatus(await this.client.GetBrowserStatusAsync(CancellationToken.None).ConfigureAwait(true));
        }
        catch (Exception exception) when (exception is IOException or SocketException or JsonException or InvalidOperationException or KeyNotFoundException)
        {
            // A daemon without the browser methods has no browser to show.
            this.IsBrowserOpen = false;
            this.WaitingPageCount = 0;
        }
    }

    [RelayCommand]
    private Task OpenDownloadPageAsync(DownloadQueueItem? download) => download is not { IsAwaitingUser: true }
        ? Task.CompletedTask
        : this.ChangeBrowserAsync(() => this.client.OpenBrowserAsync(download.Id, this.IsBrowserAutoAdvancing, CancellationToken.None), "Could not open the page");

    [RelayCommand]
    private Task NextBrowserPageAsync()
    {
        // While the browser shows a page the daemon already has the setting, and sending it alone would only change it.
        bool? autoAdvance = this.IsBrowserOpen ? null : this.IsBrowserAutoAdvancing;
        return this.ChangeBrowserAsync(() => this.client.OpenBrowserAsync(id: null, autoAdvance, CancellationToken.None), "Could not open the next page");
    }

    [RelayCommand]
    private Task SetBrowserAutoAdvanceAsync() => this.IsBrowserOpen
        ? this.ChangeBrowserAsync(() => this.client.OpenBrowserAsync(id: null, this.IsBrowserAutoAdvancing, CancellationToken.None), "Could not change auto-advance")
        : Task.CompletedTask;

    [RelayCommand]
    private Task CloseMsbeBrowserAsync() =>
        this.ChangeBrowserAsync(() => this.client.CloseBrowserAsync(CancellationToken.None), "Could not close the MSBE browser");

    /// <summary>Asks the daemon to change the MSBE browser, then shows the browser as it now is.</summary>
    private async Task ChangeBrowserAsync(Func<Task<BrowserStatusInfo>> change, string failure)
    {
        if (!this.IsDownloadQueueSupported)
        {
            return;
        }

        try
        {
            this.ApplyBrowserStatus(await change().ConfigureAwait(true));
        }
        catch (Exception exception) when (exception is IOException or SocketException or JsonException or InvalidOperationException or KeyNotFoundException)
        {
            this.StatusMessage = $"{failure}: {exception.Message}";
        }
    }

    private void ApplyBrowserStatus(BrowserStatusInfo status)
    {
        this.IsBrowserOpen = status.IsRunning;
        this.WaitingPageCount = status.Waiting;
        this.IsBrowserAutoAdvancing = status.IsAutoAdvancing;
        string position = status.Position is long at ? $"{at} of {status.Waiting}" : $"{status.Waiting} waiting";
        string shown = status.Title is { Length: > 0 } title ? title : status.Location ?? status.Page ?? string.Empty;
        this.BrowserSummary = shown.Length > 0 ? $"{status.Provider} · {position} · {shown}" : $"{status.Provider} · {position}";
        if (status.Message is { } message && !string.Equals(message, this.browserMessage, StringComparison.Ordinal))
        {
            this.StatusMessage = message;
        }

        this.browserMessage = status.Message;
    }

    [RelayCommand]
    private async Task SubmitHandoffLinkAsync()
    {
        string link = this.HandoffLink.Trim();
        if (link.Length == 0 || !this.IsDownloadQueueSupported)
        {
            return;
        }

        if (await this.SubmitLinkAsync(link).ConfigureAwait(true))
        {
            this.HandoffLink = string.Empty;
        }
    }

    /// <summary>Hands the links that arrived before the daemon connected to its download queue.</summary>
    private async Task SubmitPendingLinksAsync()
    {
        List<string> links = [.. this.pendingLinks];
        this.pendingLinks.Clear();
        foreach (string link in links)
        {
            await this.SubmitLinkAsync(link).ConfigureAwait(true);
        }
    }

    /// <summary>Hands a link to the daemon and reports what it received, never the link, whose query carries a key.</summary>
    private async Task<bool> SubmitLinkAsync(string link)
    {
        try
        {
            HandoffReceiptInfo receipt = await this.client.SubmitHandoffAsync(link, CancellationToken.None).ConfigureAwait(true);
            string received = $"{receipt.Provider}:{receipt.Project} release {receipt.Release}";
            this.StatusMessage = receipt.IsMatched
                ? $"Received {received}; its download continues."
                : $"Received {received}. Add it to a profile from Downloads.";
        }
        catch (Exception exception) when (exception is IOException or SocketException or JsonException or InvalidOperationException or KeyNotFoundException)
        {
            this.StatusMessage = $"Could not add the link: {exception.Message}";
            return false;
        }

        await this.RefreshDownloadsAsync().ConfigureAwait(true);
        return true;
    }

    [RelayCommand]
    private Task MoveDownloadUpAsync(DownloadQueueItem? download) => this.MoveQueuedDownloadAsync(download, -1);

    [RelayCommand]
    private Task MoveDownloadDownAsync(DownloadQueueItem? download) => this.MoveQueuedDownloadAsync(download, 1);

    [RelayCommand]
    private Task RemoveQueuedDownloadAsync(DownloadQueueItem? download) => download is null
        ? Task.CompletedTask
        : this.ChangeDownloadsAsync(() => this.client.CancelDownloadAsync(download.Id, CancellationToken.None), "Could not cancel the download");

    [RelayCommand]
    private Task RetryDownloadAsync(DownloadQueueItem? download) => download is not { IsFailed: true }
        ? Task.CompletedTask
        : this.ChangeDownloadsAsync(() => this.client.RetryDownloadAsync(download.Id, CancellationToken.None), "Could not retry the download");

    [RelayCommand]
    private Task AddDownloadToProfileAsync(DownloadQueueItem? download)
    {
        if (download is not { NeedsProfile: true } || this.SelectedInstance is not { } instance || this.SelectedProfile is not { } profile)
        {
            return Task.CompletedTask;
        }

        return this.ChangeDownloadsAsync(() => this.client.ConfirmDownloadAsync(download.Id, instance, profile, CancellationToken.None), "Could not add the download");
    }

    [RelayCommand]
    private Task ClearFinishedDownloadsAsync() =>
        this.ChangeDownloadsAsync(() => this.client.ClearDownloadsAsync(CancellationToken.None), "Could not clear finished downloads");

    [RelayCommand]
    private Task ToggleDownloadQueuePausedAsync() => this.IsDownloadQueuePaused
        ? this.ChangeDownloadsAsync(() => this.client.ResumeDownloadsAsync(id: null, CancellationToken.None), "Could not resume the queue")
        : this.ChangeDownloadsAsync(() => this.client.PauseDownloadsAsync(id: null, CancellationToken.None), "Could not pause the queue");

    private Task MoveQueuedDownloadAsync(DownloadQueueItem? download, int offset)
    {
        int index = download is null ? -1 : this.QueuedDownloads.IndexOf(download);
        int target = index + offset;
        if (download is null || index < 0 || target < 0 || target >= this.QueuedDownloads.Count)
        {
            return Task.CompletedTask;
        }

        int position = this.downloadOrder.IndexOf(this.QueuedDownloads[target].Id);
        return position < 0
            ? Task.CompletedTask
            : this.ChangeDownloadsAsync(() => this.client.MoveDownloadAsync(download.Id, position, CancellationToken.None), "Could not move the download");
    }

    /// <summary>Asks the daemon to change the queue, then shows the queue as it now is.</summary>
    private async Task ChangeDownloadsAsync(Func<Task> change, string failure)
    {
        if (!this.IsDownloadQueueSupported)
        {
            return;
        }

        try
        {
            await change().ConfigureAwait(true);
        }
        catch (Exception exception) when (exception is IOException or SocketException or JsonException or InvalidOperationException or KeyNotFoundException)
        {
            this.StatusMessage = $"{failure}: {exception.Message}";
            return;
        }

        await this.RefreshDownloadsAsync().ConfigureAwait(true);
    }

    private async Task ApplyDownloadsAsync(DownloadListInfo list)
    {
        bool announce = this.downloadRevision > 0;
        this.downloadRevision = list.Next;
        this.IsDownloadQueuePaused = list.IsPaused;
        List<DownloadQueueItem> added = [];
        foreach (DownloadInfo download in list.Items)
        {
            if (this.downloads.TryGetValue(download.Id, out DownloadQueueItem? item))
            {
                bool wasCompleted = item.IsCompleted;
                item.Update(download);
                if (!wasCompleted && item.IsCompleted)
                {
                    added.Add(item);
                }
            }
            else
            {
                item = new DownloadQueueItem(download, this.downloadIcons.GetValueOrDefault(download.Id));
                this.downloads[download.Id] = item;
                if (announce && item.IsCompleted)
                {
                    added.Add(item);
                }
            }
        }

        HashSet<long> listed = [.. list.Order];
        foreach (long cleared in this.downloads.Keys.Where(id => !listed.Contains(id)).ToList())
        {
            this.downloads.Remove(cleared);
            this.downloadIcons.Remove(cleared);
        }

        this.downloadOrder = [.. list.Order];
        List<DownloadQueueItem> ordered = [.. list.Order.Where(this.downloads.ContainsKey).Select(id => this.downloads[id])];
        DownloadQueueItem? active = ordered.Find(item => item.IsActive);
        this.ActiveDownload = active;
        Replace(this.QueuedDownloads, [.. ordered.Where(item => !item.IsFinished && !ReferenceEquals(item, active))]);
        Replace(this.FinishedDownloads, [.. Enumerable.Reverse(ordered).Where(item => item.IsFinished)]);
        this.NotifyDownloadsChanged();

        if (added.Count == 0)
        {
            return;
        }

        DownloadQueueItem last = added[^1];
        this.StatusMessage = $"Added {last.Title} to {last.Profile}. Review deployment to apply it.";
        if (this.SelectedInstance is { } instance && this.SelectedProfile is { } profile &&
            added.Exists(item => string.Equals(item.Instance, instance, StringComparison.Ordinal) && string.Equals(item.Profile, profile, StringComparison.Ordinal)))
        {
            await this.LoadModsAsync(instance, profile).ConfigureAwait(true);
        }
    }

    private void OnDownloadsChanged(object? sender, NotifyCollectionChangedEventArgs eventArgs) => this.NotifyDownloadsChanged();

    private void NotifyDownloadsChanged()
    {
        this.OnPropertyChanged(nameof(this.IsDownloading));
        this.OnPropertyChanged(nameof(this.PendingDownloadCount));
        this.OnPropertyChanged(nameof(this.HasPendingDownloads));
        this.OnPropertyChanged(nameof(this.HasQueuedDownloads));
        this.OnPropertyChanged(nameof(this.HasFinishedDownloads));
        this.OnPropertyChanged(nameof(this.DownloadSummary));
        this.OnPropertyChanged(nameof(this.DownloadQueueToggleLabel));
        this.OnPropertyChanged(nameof(this.DownloadIdleTitle));
        this.OnPropertyChanged(nameof(this.DownloadIdleHint));
    }
}
