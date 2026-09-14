using System.Collections.ObjectModel;
using System.Net.Sockets;
using System.Text.Json;

using CommunityToolkit.Mvvm.ComponentModel;
using CommunityToolkit.Mvvm.Input;

using MSBE.Client;
using MSBE.Desktop.Resources;

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
    public string DataDirectoryText => this.DataDirectory ?? Strings.SettingsDataDirectoryUnknown;

    /// <summary>Gets the codecs and provider programs installed in the data folder, admitted or refused.</summary>
    public ObservableCollection<InstalledExtensionItem> InstalledExtensions { get; } = [];

    /// <summary>Gets or sets a summary of the installed extensions, or why they could not be listed.</summary>
    [ObservableProperty]
    public partial string ExtensionsStatus { get; set; } = Strings.ExtensionsNotConnected;

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
            ? Strings.FormatSettingsDataFolderOpened(path)
            : Strings.FormatSettingsDataFolderNotOpened(path);
    }

    [RelayCommand]
    private async Task LoadExtensionsAsync()
    {
        if (!this.IsTypedPackSupported)
        {
            this.ExtensionsStatus = Strings.ExtensionsUnsupported;
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
                ? Strings.ExtensionsNone
                : Strings.FormatExtensionsSummary(extensions.Count(extension => extension.IsActive), extensions.Count);
        }
        catch (MsbeRpcException exception)
        {
            this.ExtensionsStatus = Strings.FormatExtensionsRefusedByDaemon(exception.Message);
        }
        catch (Exception exception) when (exception is IOException or SocketException or JsonException or InvalidOperationException)
        {
            this.ExtensionsStatus = Strings.FormatExtensionsFailed(exception.Message);
        }
    }
}
