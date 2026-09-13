using System.Collections.ObjectModel;
using System.Net.Sockets;
using System.Text.Json;

using CommunityToolkit.Mvvm.ComponentModel;
using CommunityToolkit.Mvvm.Input;

using MSBE.Client;

namespace MSBE.Desktop.ViewModels;

/// <content>Settings and shortcuts into MSBE's own state.</content>
internal sealed partial class MainViewModel
{
    /// <summary>Gets or sets the data directory reported by the connected daemon.</summary>
    [ObservableProperty]
    [NotifyPropertyChangedFor(nameof(DataDirectoryText))]
    [NotifyCanExecuteChangedFor(nameof(OpenDataDirectoryCommand))]
    public partial string? DataDirectory { get; set; }

    /// <summary>Gets the data directory, or why it is unknown.</summary>
    public string DataDirectoryText => this.DataDirectory ?? "Not reported by the daemon.";

    /// <summary>Gets the codecs and provider programs installed in the data folder, admitted or refused.</summary>
    public ObservableCollection<InstalledExtensionItem> InstalledExtensions { get; } = [];

    /// <summary>Gets or sets a summary of the installed extensions, or why they could not be listed.</summary>
    [ObservableProperty]
    public partial string ExtensionsStatus { get; set; } = "Connect to a daemon to list installed extensions.";

    private bool CanOpenDataDirectory => this.DataDirectory is not null && this.folders is not null;

    [RelayCommand(CanExecute = nameof(CanOpenDataDirectory))]
    private async Task OpenDataDirectoryAsync()
    {
        if (this.DataDirectory is not { } path || this.folders is null)
        {
            return;
        }

        bool opened = await this.folders.OpenAsync(path).ConfigureAwait(true);
        this.StatusMessage = opened
            ? $"Opened data folder {path}."
            : $"Could not open {path}. MSBE creates it when the first instance is added.";
    }

    [RelayCommand]
    private async Task LoadExtensionsAsync()
    {
        if (!this.IsTypedPackSupported)
        {
            this.ExtensionsStatus = "This daemon cannot list installed extensions; restart MSBE to update it.";
            return;
        }

        try
        {
            IReadOnlyList<ExtensionInfo> extensions = await this.client.ListExtensionsAsync(CancellationToken.None).ConfigureAwait(true);
            this.InstalledExtensions.Clear();
            foreach (ExtensionInfo extension in extensions)
            {
                this.InstalledExtensions.Add(InstalledExtensionItem.From(extension));
            }

            this.ExtensionsStatus = extensions.Count == 0
                ? "No extensions are installed. Signed codecs and provider programs are copied into the extensions folder inside the data folder."
                : $"{extensions.Count(extension => extension.IsActive)} of {extensions.Count} installed extensions are active.";
        }
        catch (MsbeRpcException exception)
        {
            this.ExtensionsStatus = $"This daemon cannot list installed extensions: {exception.Message}";
        }
        catch (Exception exception) when (exception is IOException or SocketException or JsonException or InvalidOperationException)
        {
            this.ExtensionsStatus = $"Could not list installed extensions: {exception.Message}";
        }
    }
}
