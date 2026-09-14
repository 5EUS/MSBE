using System.Collections.ObjectModel;
using System.Net.Sockets;

using CommunityToolkit.Mvvm.ComponentModel;

using MSBE.Client;
using MSBE.Desktop.Resources;
using MSBE.Desktop.Services;

namespace MSBE.Desktop.ViewModels;

/// <summary>
/// The shell view model.
/// </summary>
/// <remarks>
/// Split across <c>MainViewModel.*.cs</c> partials, one per feature area, so that the
/// shell stays navigable as it grows. Members belong in the partial named for the
/// surface they serve, never here — this file holds only shell-wide state.
/// </remarks>
internal sealed partial class MainViewModel : ViewModelBase
{
    private readonly IMsbeClient client;
    private readonly IFolderLauncher? folders;
    private readonly ILinkLauncher? links;
    private readonly TimeProvider time;

    /// <summary>Initializes a new instance of the <see cref="MainViewModel" /> class.</summary>
    /// <param name="client">The client used for daemon-owned operations.</param>
    /// <param name="folders">Shows folders in the file manager, or <see langword="null" /> where there is none.</param>
    /// <param name="time">The clock ages are measured against, or <see langword="null" /> for the system clock.</param>
    /// <param name="links">Opens web pages in the user's own browser, or <see langword="null" /> where there is none.</param>
    public MainViewModel(IMsbeClient client, IFolderLauncher? folders = null, TimeProvider? time = null, ILinkLauncher? links = null)
    {
        this.client = client;
        this.folders = folders;
        this.links = links;
        this.time = time ?? TimeProvider.System;
        this.BrowseResults.CollectionChanged += this.OnBrowseResultsChanged;
        this.QueuedDownloads.CollectionChanged += this.OnDownloadsChanged;
        this.FinishedDownloads.CollectionChanged += this.OnDownloadsChanged;
    }

    /// <summary>Gets games currently supported by the connected daemon.</summary>
    public ObservableCollection<GameInfo> Games { get; } = [];

    /// <summary>Gets a value indicating whether the daemon reported no supported games.</summary>
    public bool IsGamesEmpty => this.Games.Count == 0;

    /// <summary>Gets or sets the window title.</summary>
    [ObservableProperty]
    public partial string Title { get; set; } = "MSBE";

    /// <summary>Gets or sets the message shown in the status bar.</summary>
    [ObservableProperty]
    public partial string StatusMessage { get; set; } = Strings.ShellNotConnected;

    /// <summary>Connects to the local daemon and updates the shell status.</summary>
    /// <returns>A task that completes after the connection attempt.</returns>
    public async Task ConnectAsync()
    {
        try
        {
            DaemonInfo daemon = await this.client.GetInfoAsync(CancellationToken.None).ConfigureAwait(true);
            this.IsPackConfigurationSupported = daemon.RpcVersion >= 3;
            this.IsTypedPackSupported = daemon.RpcVersion >= 4;
            this.IsDownloadQueueSupported = daemon.RpcVersion >= 5;
            this.DataDirectory = daemon.DataDirectory;
            IReadOnlyList<GameInfo> games = await this.client.GetGamesAsync(CancellationToken.None).ConfigureAwait(true);
            this.Games.Clear();
            foreach (GameInfo game in games)
            {
                this.Games.Add(game);
            }

            this.RefreshGameSearch();
            this.OnPropertyChanged(nameof(this.IsGamesEmpty));

            await this.RefreshInstancesAsync().ConfigureAwait(true);
            await this.LoadExportCodecsAsync().ConfigureAwait(true);
            await this.LoadExtensionsAsync().ConfigureAwait(true);
            if (this.IsDownloadQueueSupported)
            {
                await this.LoadIntegrationsAsync().ConfigureAwait(true);
                this.StartDownloadPolling();
            }

            this.StatusMessage = this.IsPackConfigurationSupported
                ? Strings.FormatShellConnected(daemon.Version, daemon.RpcVersion)
                : Strings.FormatShellDaemonOutdated(daemon.RpcVersion);
            if (this.IsDownloadQueueSupported)
            {
                await this.SubmitPendingLinksAsync().ConfigureAwait(true);
            }
        }
        catch (Exception exception) when (exception is IOException or SocketException or InvalidOperationException)
        {
            this.StatusMessage = Strings.FormatShellDaemonUnavailable(exception.Message);
        }
    }
}
