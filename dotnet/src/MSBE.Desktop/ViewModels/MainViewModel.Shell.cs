using System.Net.Sockets;

using CommunityToolkit.Mvvm.ComponentModel;
using CommunityToolkit.Mvvm.Input;

using MSBE.Client;
using MSBE.Desktop.Resources;

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
    public partial string InstanceSort { get; set; } = Strings.InstanceSortNameAscending;

    /// <summary>Gets or sets the command staged in the CLI.</summary>
    [ObservableProperty]
    public partial string CliInput { get; set; } = string.Empty;

    /// <summary>Gets or sets the daemon command transcript.</summary>
    [ObservableProperty]
    public partial string CliTranscript { get; set; } = string.Empty;

    /// <summary>Gets the title shown above the active workspace.</summary>
    public string ActiveWorkspaceTitle => this.ActiveWorkspace switch
    {
        WorkspacePage.Instances => Strings.WorkspaceInstances,
        WorkspacePage.Games => Strings.WorkspaceGames,
        WorkspacePage.Browse => Strings.WorkspaceBrowse,
        WorkspacePage.Pack => Strings.WorkspacePack,
        WorkspacePage.Downloads => Strings.WorkspaceDownloads,
        WorkspacePage.History => Strings.WorkspaceHistory,
        WorkspacePage.Settings => Strings.WorkspaceSettings,
        _ => "MSBE",
    };

    /// <summary>Gets a value indicating whether the instances workspace is active.</summary>
    public bool IsInstancesWorkspace => this.ActiveWorkspace == WorkspacePage.Instances;

    /// <summary>Gets a value indicating whether the browse workspace is active.</summary>
    public bool IsBrowseWorkspace => this.ActiveWorkspace == WorkspacePage.Browse;

    /// <summary>Gets a value indicating whether the supported games workspace is active.</summary>
    public bool IsGamesWorkspace => this.ActiveWorkspace == WorkspacePage.Games;

    /// <summary>Gets a value indicating whether the pack workspace is active.</summary>
    public bool IsPackWorkspace => this.ActiveWorkspace == WorkspacePage.Pack;

    /// <summary>Gets a value indicating whether the downloads workspace is active.</summary>
    public bool IsDownloadsWorkspace => this.ActiveWorkspace == WorkspacePage.Downloads;

    /// <summary>Gets a value indicating whether the history workspace is active.</summary>
    public bool IsHistoryWorkspace => this.ActiveWorkspace == WorkspacePage.History;

    /// <summary>Gets a value indicating whether the settings workspace is active.</summary>
    public bool IsSettingsWorkspace => this.ActiveWorkspace == WorkspacePage.Settings;

    /// <summary>Gets a value indicating whether a not-yet-implemented workspace is active.</summary>
    public bool IsPlaceholderWorkspace => !this.IsInstancesWorkspace && !this.IsBrowseWorkspace && !this.IsGamesWorkspace && !this.IsPackWorkspace && !this.IsDownloadsWorkspace && !this.IsHistoryWorkspace && !this.IsSettingsWorkspace;

    /// <summary>Gets the available instance library sort orders.</summary>
    public IReadOnlyList<string> InstanceSorts { get; } = [Strings.InstanceSortNameAscending, Strings.InstanceSortNameDescending];

    private static string[] SplitArguments(string command) => command.Split(' ', StringSplitOptions.RemoveEmptyEntries | StringSplitOptions.TrimEntries);

    partial void OnActiveWorkspaceChanged(WorkspacePage value)
    {
        this.OnPropertyChanged(nameof(this.ActiveWorkspaceTitle));
        this.OnPropertyChanged(nameof(this.IsInstancesWorkspace));
        this.OnPropertyChanged(nameof(this.IsBrowseWorkspace));
        this.OnPropertyChanged(nameof(this.IsGamesWorkspace));
        this.OnPropertyChanged(nameof(this.IsPackWorkspace));
        this.OnPropertyChanged(nameof(this.IsDownloadsWorkspace));
        this.OnPropertyChanged(nameof(this.IsHistoryWorkspace));
        this.OnPropertyChanged(nameof(this.IsSettingsWorkspace));
        this.OnPropertyChanged(nameof(this.IsPlaceholderWorkspace));
        if (value == WorkspacePage.History)
        {
            _ = this.LoadHistoryAsync();
        }
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
            this.CliTranscript += Strings.FormatCliExitCode(result.ExitCode);
        }
        catch (Exception exception) when (exception is IOException or SocketException or InvalidOperationException)
        {
            this.CliTranscript += Strings.FormatShellDaemonUnavailable(exception.Message);
        }
    }
}
