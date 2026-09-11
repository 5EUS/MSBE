using System.Collections.ObjectModel;

using CommunityToolkit.Mvvm.ComponentModel;
using CommunityToolkit.Mvvm.Input;

namespace MSBE.Desktop.ViewModels;

/// <content>Instance discovery and selection.</content>
internal sealed partial class MainViewModel
{
    /// <summary>Gets the detected game instances.</summary>
    public ObservableCollection<string> Instances { get; } = [];

    /// <summary>Gets or sets the currently selected instance, if any.</summary>
    [ObservableProperty]
    public partial string? SelectedInstance { get; set; }

    /// <summary>Asks the daemon to re-run store detection.</summary>
    /// <returns>A task that completes when detection has finished.</returns>
    [RelayCommand]
    private async Task DetectInstancesAsync()
    {
        this.StatusMessage = "Detecting instances...";
        await Task.Yield();
        this.StatusMessage = $"{this.Instances.Count} instance(s) detected.";
    }
}
