using System.Collections.ObjectModel;
using System.Net.Sockets;
using System.Text.Json;

using CommunityToolkit.Mvvm.ComponentModel;
using CommunityToolkit.Mvvm.Input;

using MSBE.Client;

namespace MSBE.Desktop.ViewModels;

/// <content>Deployment review and execution for the selected profile.</content>
internal sealed partial class MainViewModel
{
    /// <summary>Gets the planned filesystem changes.</summary>
    public ObservableCollection<DeploymentPreviewItem> DeploymentChanges { get; } = [];

    /// <summary>Gets files deliberately excluded by the plan.</summary>
    public ObservableCollection<DeploymentExclusionItem> DeploymentExclusions { get; } = [];

    /// <summary>Gets or sets whether the deployment review window is open.</summary>
    [ObservableProperty]
    public partial bool IsDeploymentPreviewOpen { get; set; }

    /// <summary>Gets or sets whether a deployment plan or apply operation is running.</summary>
    [ObservableProperty]
    public partial bool IsDeploymentBusy { get; set; }

    /// <summary>Gets or sets an error shown in the deployment review.</summary>
    [ObservableProperty]
    [NotifyPropertyChangedFor(nameof(HasDeploymentError))]
    public partial string DeploymentError { get; set; } = string.Empty;

    /// <summary>Gets or sets the number of already-current managed files.</summary>
    [ObservableProperty]
    public partial int DeploymentUnchangedCount { get; set; }

    /// <summary>Gets or sets the number of locally modified defaults that will be kept.</summary>
    [ObservableProperty]
    public partial int DeploymentKeptCount { get; set; }

    /// <summary>Gets a value indicating whether deployment review has an error.</summary>
    public bool HasDeploymentError => !string.IsNullOrEmpty(this.DeploymentError);

    private static string DescribeExclusion(JsonElement reason)
    {
        string kind = reason.GetProperty("kind").GetString() ?? "excluded";
        return reason.TryGetProperty("pattern", out JsonElement pattern)
            ? $"{kind.Replace('_', ' ')} - {pattern.GetString()}"
            : kind.Replace('_', ' ');
    }

    [RelayCommand]
    private async Task PreviewDeploymentAsync()
    {
        if (this.SelectedInstance is null || this.SelectedProfile is null)
        {
            return;
        }

        this.IsDeploymentPreviewOpen = true;
        this.IsDeploymentBusy = true;
        this.DeploymentError = string.Empty;
        this.DeploymentChanges.Clear();
        this.DeploymentExclusions.Clear();
        try
        {
            CommandResult result = await this.client.RunCommandAsync(
                ["--format", "json", "deploy", this.SelectedInstance, "--profile", this.SelectedProfile, "--dry-run"],
                CancellationToken.None).ConfigureAwait(true);
            if (result.ExitCode != 0)
            {
                throw new InvalidOperationException(result.StandardError.Trim());
            }

            using JsonDocument document = JsonDocument.Parse(result.StandardOutput);
            this.ParseDeploymentPlan(document.RootElement);
        }
        catch (Exception exception) when (exception is IOException or SocketException or JsonException or InvalidOperationException)
        {
            this.DeploymentError = exception.Message;
        }
        finally
        {
            this.IsDeploymentBusy = false;
        }
    }

    [RelayCommand]
    private void CancelDeploymentPreview() => this.IsDeploymentPreviewOpen = false;

    [RelayCommand]
    private async Task ApplyDeploymentAsync()
    {
        if (this.SelectedInstance is null || this.SelectedProfile is null || this.IsDeploymentBusy)
        {
            return;
        }

        this.IsDeploymentBusy = true;
        this.DeploymentError = string.Empty;
        try
        {
            string instance = this.SelectedInstance;
            CommandResult result = await this.client.RunCommandAsync(
                ["--format", "json", "deploy", instance, "--profile", this.SelectedProfile],
                CancellationToken.None).ConfigureAwait(true);
            if (result.ExitCode != 0)
            {
                throw new InvalidOperationException(result.StandardError.Trim());
            }

            this.IsDeploymentPreviewOpen = false;
            await this.LoadSelectedInstanceAsync(instance).ConfigureAwait(true);
            this.StatusMessage = $"Deployed {this.SelectedProfile} to {instance}.";
        }
        catch (Exception exception) when (exception is IOException or SocketException or InvalidOperationException)
        {
            this.DeploymentError = exception.Message;
        }
        finally
        {
            this.IsDeploymentBusy = false;
        }
    }

    private void ParseDeploymentPlan(JsonElement plan)
    {
        foreach (JsonElement operation in plan.GetProperty("operations").EnumerateArray())
        {
            string kind = operation.GetProperty("op").GetString() ?? "change";
            string path = operation.GetProperty("path").GetString() ?? string.Empty;
            (string action, string marker) = kind switch
            {
                "materialize" => ("Place", "+"),
                "remove" => ("Remove", "-"),
                "create_dir" => ("Create folder", "+"),
                "remove_dir" => ("Remove folder", "-"),
                _ => ("Change", "*"),
            };
            this.DeploymentChanges.Add(new DeploymentPreviewItem(action, path, marker));
        }

        foreach (JsonElement exclusion in plan.GetProperty("excluded").EnumerateArray())
        {
            JsonElement file = exclusion.GetProperty("file");
            this.DeploymentExclusions.Add(new DeploymentExclusionItem(
                exclusion.GetProperty("module").GetString() ?? string.Empty,
                file.GetProperty("source").GetString() ?? string.Empty,
                DescribeExclusion(file.GetProperty("reason"))));
        }

        this.DeploymentUnchangedCount = plan.GetProperty("unchanged").GetInt32();
        this.DeploymentKeptCount = plan.GetProperty("kept").GetArrayLength();
    }
}
