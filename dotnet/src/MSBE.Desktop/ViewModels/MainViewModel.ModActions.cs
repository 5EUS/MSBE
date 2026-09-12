using System.Net.Sockets;
using System.Text.Json;

using CommunityToolkit.Mvvm.ComponentModel;
using CommunityToolkit.Mvvm.Input;

using MSBE.Client;

namespace MSBE.Desktop.ViewModels;

/// <content>Profile composition and latest-deployment rollback.</content>
internal sealed partial class MainViewModel
{
    /// <summary>Gets or sets a local path, URL, or provider reference to add.</summary>
    [ObservableProperty]
    public partial string ModSource { get; set; } = string.Empty;

    /// <summary>Gets or sets whether required provider dependencies are included.</summary>
    [ObservableProperty]
    public partial bool AddModWithDependencies { get; set; } = true;

    /// <summary>Gets or sets whether a mod mutation is running.</summary>
    [ObservableProperty]
    public partial bool IsModMutationBusy { get; set; }

    /// <summary>Gets or sets whether rollback is running.</summary>
    [ObservableProperty]
    [NotifyPropertyChangedFor(nameof(CanRollbackLatest))]
    public partial bool IsRollbackBusy { get; set; }

    [RelayCommand]
    private async Task AddModSourceAsync()
    {
        if (this.SelectedInstance is null || this.SelectedProfile is null || string.IsNullOrWhiteSpace(this.ModSource) || this.IsModMutationBusy)
        {
            return;
        }

        this.IsModMutationBusy = true;
        this.ModsError = string.Empty;
        try
        {
            string source = this.ModSource.Trim();
            List<string> arguments = ["--format", "json", "add", this.SelectedInstance, source, "--profile", this.SelectedProfile];
            if (this.AddModWithDependencies)
            {
                arguments.Add("--with-deps");
            }

            CommandResult result = await this.client.RunCommandAsync(arguments, CancellationToken.None).ConfigureAwait(true);
            if (result.ExitCode != 0)
            {
                throw new InvalidOperationException(result.StandardError.Trim());
            }

            await this.LoadModsAsync(this.SelectedInstance, this.SelectedProfile).ConfigureAwait(true);
            this.ModSource = string.Empty;
            this.StatusMessage = $"Added {source} to {this.SelectedProfile}. Review deployment to apply it.";
        }
        catch (Exception exception) when (exception is IOException or SocketException or InvalidOperationException)
        {
            this.ModsError = exception.Message;
        }
        finally
        {
            this.IsModMutationBusy = false;
        }
    }

    [RelayCommand]
    private async Task RemoveSelectedModAsync()
    {
        if (this.SelectedInstance is null || this.SelectedProfile is null || this.SelectedMod is null || this.IsModMutationBusy)
        {
            return;
        }

        this.IsModMutationBusy = true;
        this.ModsError = string.Empty;
        try
        {
            string module = this.SelectedMod.Name;
            CommandResult result = await this.client.RunCommandAsync(
                ["--format", "json", "remove", this.SelectedInstance, module, "--profile", this.SelectedProfile],
                CancellationToken.None).ConfigureAwait(true);
            if (result.ExitCode != 0)
            {
                throw new InvalidOperationException(result.StandardError.Trim());
            }

            await this.LoadModsAsync(this.SelectedInstance, this.SelectedProfile).ConfigureAwait(true);
            this.StatusMessage = $"Removed {module} from {this.SelectedProfile}. Review deployment to apply it.";
        }
        catch (Exception exception) when (exception is IOException or SocketException or InvalidOperationException)
        {
            this.ModsError = exception.Message;
        }
        finally
        {
            this.IsModMutationBusy = false;
        }
    }

    [RelayCommand]
    private async Task RollbackLatestAsync()
    {
        if (this.SelectedInstance is null || this.DeployedProfile is null || this.IsRollbackBusy)
        {
            return;
        }

        this.IsRollbackBusy = true;
        this.InstanceError = string.Empty;
        try
        {
            string instance = this.SelectedInstance;
            CommandResult result = await this.client.RunCommandAsync(
                ["--format", "json", "rollback", instance],
                CancellationToken.None).ConfigureAwait(true);
            if (result.ExitCode != 0)
            {
                throw new InvalidOperationException(result.StandardError.Trim());
            }

            using JsonDocument document = JsonDocument.Parse(result.StandardOutput);
            string message = document.RootElement.GetProperty("rolled_back").ValueKind == JsonValueKind.String
                ? "Rolled back the latest deployment."
                : "There was no deployment to roll back.";
            await this.LoadSelectedInstanceAsync(instance).ConfigureAwait(true);
            await this.LoadProfilesAsync(instance).ConfigureAwait(true);
            this.StatusMessage = message;
        }
        catch (Exception exception) when (exception is IOException or SocketException or JsonException or InvalidOperationException)
        {
            this.InstanceError = exception.Message;
        }
        finally
        {
            this.IsRollbackBusy = false;
        }
    }
}
