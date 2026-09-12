using CommunityToolkit.Mvvm.ComponentModel;
using CommunityToolkit.Mvvm.Input;

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
}
