using System.Net.Sockets;

using CommunityToolkit.Mvvm.ComponentModel;
using CommunityToolkit.Mvvm.Input;

using MSBE.Client;

namespace MSBE.Desktop.ViewModels;

/// <content>Shell navigation and the daemon-backed command surface.</content>
internal sealed partial class MainViewModel
{
    /// <summary>Gets or sets the workspace currently shown in the center area.</summary>
    [ObservableProperty]
    public partial WorkspacePage ActiveWorkspace { get; set; } = WorkspacePage.Instances;

    /// <summary>Gets or sets whether the instance library sidebar is expanded.</summary>
    [ObservableProperty]
    public partial bool IsSidebarExpanded { get; set; } = true;

    /// <summary>Gets or sets whether the command window is open.</summary>
    [ObservableProperty]
    public partial bool IsCliOpen { get; set; }

    /// <summary>Gets or sets text used to filter the instance library.</summary>
    [ObservableProperty]
    public partial string InstanceSearchText { get; set; } = string.Empty;

    /// <summary>Gets or sets the selected presentation order for the instance library.</summary>
    [ObservableProperty]
    public partial string InstanceSort { get; set; } = "Name (A-Z)";

    /// <summary>Gets or sets the command staged in the CLI.</summary>
    [ObservableProperty]
    public partial string CliInput { get; set; } = string.Empty;

    /// <summary>Gets or sets the daemon command transcript.</summary>
    [ObservableProperty]
    public partial string CliTranscript { get; set; } = string.Empty;

    /// <summary>Gets the title shown above the active workspace.</summary>
    public string ActiveWorkspaceTitle => this.ActiveWorkspace switch
    {
        WorkspacePage.Instances => "Instances",
        WorkspacePage.Games => "Games",
        WorkspacePage.Providers => "Providers",
        WorkspacePage.Settings => "Settings",
        _ => "MSBE",
    };

    /// <summary>Gets a value indicating whether the instances workspace is active.</summary>
    public bool IsInstancesWorkspace => this.ActiveWorkspace == WorkspacePage.Instances;

    /// <summary>Gets the available instance library sort orders.</summary>
    public IReadOnlyList<string> InstanceSorts { get; } = ["Name (A-Z)", "Name (Z-A)"];

    private static string[] SplitArguments(string command) => command.Split(' ', StringSplitOptions.RemoveEmptyEntries | StringSplitOptions.TrimEntries);

    partial void OnActiveWorkspaceChanged(WorkspacePage value)
    {
        this.OnPropertyChanged(nameof(this.ActiveWorkspaceTitle));
        this.OnPropertyChanged(nameof(this.IsInstancesWorkspace));
    }

    [RelayCommand]
    private void Navigate(WorkspacePage workspace) => this.ActiveWorkspace = workspace;

    [RelayCommand]
    private void ToggleSidebar() => this.IsSidebarExpanded = !this.IsSidebarExpanded;

    [RelayCommand]
    private void ToggleCli() => this.IsCliOpen = !this.IsCliOpen;

    [RelayCommand]
    private void CloseCli() => this.IsCliOpen = false;

    [RelayCommand]
    private async Task SubmitCliAsync()
    {
        if (string.IsNullOrWhiteSpace(this.CliInput))
        {
            return;
        }

        string command = this.CliInput;
        this.CliInput = string.Empty;
        this.CliTranscript = $"> {command}\n";
        try
        {
            CommandResult result = await this.client.RunCommandAsync(SplitArguments(command), CancellationToken.None).ConfigureAwait(false);
            this.CliTranscript += result.StandardOutput;
            this.CliTranscript += result.StandardError;
            this.CliTranscript += $"\nExit code: {result.ExitCode}";
        }
        catch (Exception exception) when (exception is IOException or SocketException or InvalidOperationException)
        {
            this.CliTranscript += $"Daemon unavailable: {exception.Message}";
        }
    }
}
