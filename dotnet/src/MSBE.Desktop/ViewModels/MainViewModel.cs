using System.Collections.ObjectModel;
using System.Net.Sockets;

using CommunityToolkit.Mvvm.ComponentModel;

using MSBE.Client;

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

    /// <summary>Initializes a new instance of the <see cref="MainViewModel" /> class.</summary>
    /// <param name="client">The client used for daemon-owned operations.</param>
    public MainViewModel(IMsbeClient client) => this.client = client;

    /// <summary>Gets games currently supported by the connected daemon.</summary>
    public ObservableCollection<GameInfo> Games { get; } = [];

    /// <summary>Gets a value indicating whether the daemon reported no supported games.</summary>
    public bool IsGamesEmpty => this.Games.Count == 0;

    /// <summary>Gets or sets the window title.</summary>
    [ObservableProperty]
    public partial string Title { get; set; } = "MSBE";

    /// <summary>Gets or sets the message shown in the status bar.</summary>
    [ObservableProperty]
    public partial string StatusMessage { get; set; } = "Not connected to a daemon.";

    /// <summary>Connects to the local daemon and updates the shell status.</summary>
    /// <returns>A task that completes after the connection attempt.</returns>
    public async Task ConnectAsync()
    {
        try
        {
            DaemonInfo daemon = await this.client.GetInfoAsync(CancellationToken.None).ConfigureAwait(true);
            IReadOnlyList<GameInfo> games = await this.client.GetGamesAsync(CancellationToken.None).ConfigureAwait(true);
            this.Games.Clear();
            foreach (GameInfo game in games)
            {
                this.Games.Add(game);
            }

            this.RefreshGameSearch();
            this.OnPropertyChanged(nameof(this.IsGamesEmpty));

            this.StatusMessage = $"Connected to daemon {daemon.Version} (RPC {daemon.RpcVersion}).";
            await this.RefreshInstancesAsync().ConfigureAwait(true);
        }
        catch (Exception exception) when (exception is IOException or SocketException or InvalidOperationException)
        {
            this.StatusMessage = $"Daemon unavailable: {exception.Message}";
        }
    }
}
