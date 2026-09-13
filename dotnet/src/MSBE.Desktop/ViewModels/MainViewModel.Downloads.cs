using System.Collections.ObjectModel;
using System.Collections.Specialized;
using System.Net.Sockets;
using System.Text.Json;

using CommunityToolkit.Mvvm.ComponentModel;
using CommunityToolkit.Mvvm.Input;

using MSBE.Client;

namespace MSBE.Desktop.ViewModels;

/// <content>The download queue, which adds provider projects to profiles one at a time.</content>
internal sealed partial class MainViewModel
{
    private Task downloadWorker = Task.CompletedTask;
    private bool isDownloadWorkerRunning;

    /// <summary>Gets downloads waiting to start, in the order they will run.</summary>
    public ObservableCollection<DownloadQueueItem> QueuedDownloads { get; } = [];

    /// <summary>Gets finished downloads, newest first.</summary>
    public ObservableCollection<DownloadQueueItem> FinishedDownloads { get; } = [];

    /// <summary>Gets or sets the download that is running.</summary>
    [ObservableProperty]
    public partial DownloadQueueItem? ActiveDownload { get; set; }

    /// <summary>Gets or sets whether the queue holds off starting the next download.</summary>
    [ObservableProperty]
    public partial bool IsDownloadQueuePaused { get; set; }

    /// <summary>Gets a value indicating whether a download is running.</summary>
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
        ({ } active, 0) => $"Downloading {active.Title}",
        ({ } active, _) when this.IsDownloadQueuePaused => $"Downloading {active.Title} · queue paused",
        ({ } active, int queued) => $"Downloading {active.Title} · {queued} queued",
    };

    /// <summary>Gets the label of the pause toggle.</summary>
    public string DownloadQueueToggleLabel => this.IsDownloadQueuePaused ? "Resume queue" : "Pause queue";

    /// <summary>Gets the heading shown while nothing is downloading.</summary>
    public string DownloadIdleTitle => this.IsDownloadQueuePaused && this.HasQueuedDownloads ? "Queue paused" : "Nothing downloading";

    /// <summary>Gets the hint shown while nothing is downloading.</summary>
    public string DownloadIdleHint => this.IsDownloadQueuePaused && this.HasQueuedDownloads
        ? "Resume the queue to start the next download."
        : "Mods you install from Browse line up here and download one at a time.";

    /// <summary>Gets a task that completes once the queue has nothing it may run.</summary>
    internal Task DownloadsSettled => this.downloadWorker;

    private static string DescribeAddition(string output)
    {
        try
        {
            using JsonDocument document = JsonDocument.Parse(output);
            JsonElement report = document.RootElement;
            int added = ArrayLength(report, "added");
            int unresolved = ArrayLength(report, "unresolved") + ArrayLength(report, "incompatible");
            string summary = (added, ArrayLength(report, "skipped")) switch
            {
                (0, > 0) => "Already in profile",
                (0, _) => "Nothing was added",
                (1, _) => "Added 1 mod",
                _ => $"Added {added} mods",
            };
            return unresolved == 0 ? summary : $"{summary} · {unresolved} unresolved";
        }
        catch (JsonException)
        {
            return "Added";
        }
    }

    private static int ArrayLength(JsonElement report, string property) =>
        report.ValueKind == JsonValueKind.Object && report.TryGetProperty(property, out JsonElement value) && value.ValueKind == JsonValueKind.Array
            ? value.GetArrayLength()
            : 0;

    partial void OnActiveDownloadChanged(DownloadQueueItem? value) => this.NotifyDownloadsChanged();

    partial void OnIsDownloadQueuePausedChanged(bool value) => this.NotifyDownloadsChanged();

    [RelayCommand]
    private void MoveDownloadUp(DownloadQueueItem? download) => this.MoveQueuedDownload(download, -1);

    [RelayCommand]
    private void MoveDownloadDown(DownloadQueueItem? download) => this.MoveQueuedDownload(download, 1);

    [RelayCommand]
    private void RemoveQueuedDownload(DownloadQueueItem? download)
    {
        if (download is not null)
        {
            this.QueuedDownloads.Remove(download);
        }
    }

    [RelayCommand]
    private void RetryDownload(DownloadQueueItem? download)
    {
        if (download is not { IsFailed: true } || this.IsDownloadQueued(download))
        {
            return;
        }

        this.FinishedDownloads.Remove(download);
        download.State = DownloadState.Queued;
        download.Detail = string.Empty;
        this.QueuedDownloads.Add(download);
        this.StartDownloads();
    }

    [RelayCommand]
    private void ClearFinishedDownloads() => this.FinishedDownloads.Clear();

    [RelayCommand]
    private void ToggleDownloadQueuePaused()
    {
        this.IsDownloadQueuePaused = !this.IsDownloadQueuePaused;
        this.StartDownloads();
    }

    /// <summary>Queues a download unless the same project is already headed for the same profile.</summary>
    /// <param name="download">The download to queue.</param>
    /// <returns><see langword="true" /> if the download was queued.</returns>
    /// <remarks>Call <see cref="StartDownloads" /> once everything is queued.</remarks>
    private bool TryQueueDownload(DownloadQueueItem download)
    {
        if (this.IsDownloadQueued(download))
        {
            return false;
        }

        this.QueuedDownloads.Add(download);
        return true;
    }

    private bool IsDownloadQueued(DownloadQueueItem download) =>
        (this.ActiveDownload is { } active && active.Targets(download)) || this.QueuedDownloads.Any(download.Targets);

    private void MoveQueuedDownload(DownloadQueueItem? download, int offset)
    {
        int index = download is null ? -1 : this.QueuedDownloads.IndexOf(download);
        int target = index + offset;
        if (index >= 0 && target >= 0 && target < this.QueuedDownloads.Count)
        {
            this.QueuedDownloads.Move(index, target);
        }
    }

    /// <summary>Starts working through the queue unless it is paused, empty, or already running.</summary>
    private void StartDownloads()
    {
        if (this.isDownloadWorkerRunning || this.IsDownloadQueuePaused || this.QueuedDownloads.Count == 0)
        {
            return;
        }

        this.isDownloadWorkerRunning = true;
        this.downloadWorker = this.RunDownloadsAsync();
    }

    private async Task RunDownloadsAsync()
    {
        try
        {
            while (!this.IsDownloadQueuePaused && this.QueuedDownloads.Count > 0)
            {
                DownloadQueueItem download = this.QueuedDownloads[0];
                this.QueuedDownloads.RemoveAt(0);
                download.State = DownloadState.Downloading;
                this.ActiveDownload = download;
                await this.DownloadAsync(download).ConfigureAwait(true);
                this.ActiveDownload = null;
                this.FinishedDownloads.Insert(0, download);
            }
        }
        finally
        {
            this.isDownloadWorkerRunning = false;
        }
    }

    private async Task DownloadAsync(DownloadQueueItem download)
    {
        try
        {
            List<string> arguments = ["--format", "json", "add", download.Instance, download.Source, "--profile", download.Profile];
            if (download.WithDependencies)
            {
                arguments.Add("--with-deps");
            }

            CommandResult result = await this.client.RunCommandAsync(arguments, CancellationToken.None).ConfigureAwait(true);
            if (result.ExitCode != 0)
            {
                string error = result.StandardError.Trim();
                throw new InvalidOperationException(error.Length > 0 ? error : $"The add command exited with code {result.ExitCode}.");
            }

            download.Detail = DescribeAddition(result.StandardOutput);
            download.State = DownloadState.Completed;
            this.StatusMessage = $"Added {download.Title} to {download.Profile}. Review deployment to apply it.";
        }
        catch (Exception exception) when (exception is IOException or SocketException or InvalidOperationException)
        {
            download.Detail = exception.Message;
            download.State = DownloadState.Failed;
            this.StatusMessage = $"Could not download {download.Title}.";
            return;
        }

        if (string.Equals(this.SelectedInstance, download.Instance, StringComparison.Ordinal) &&
            string.Equals(this.SelectedProfile, download.Profile, StringComparison.Ordinal))
        {
            await this.LoadModsAsync(download.Instance, download.Profile).ConfigureAwait(true);
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
