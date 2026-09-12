using System.Collections.ObjectModel;
using System.Net.Sockets;
using System.Text.Json;

using CommunityToolkit.Mvvm.ComponentModel;
using CommunityToolkit.Mvvm.Input;

using MSBE.Client;

namespace MSBE.Desktop.ViewModels;

/// <content>Pack-owned configuration and validation.</content>
internal sealed partial class MainViewModel
{
    /// <summary>Gets or sets whether the connected daemon supports pack configuration.</summary>
    [ObservableProperty]
    public partial bool IsPackConfigurationSupported { get; set; } = true;

    /// <summary>Gets the pack-owned configs in the selected profile.</summary>
    public ObservableCollection<PackConfigItem> PackConfigs { get; } = [];

    /// <summary>Gets or sets the config selected for editing.</summary>
    [ObservableProperty]
    public partial PackConfigItem? SelectedPackConfig { get; set; }

    /// <summary>Gets or sets the game-relative path of the config being edited.</summary>
    [ObservableProperty]
    public partial string PackConfigPath { get; set; } = string.Empty;

    /// <summary>Gets or sets the text content of the config being edited.</summary>
    [ObservableProperty]
    public partial string PackConfigContent { get; set; } = string.Empty;

    /// <summary>Gets or sets the latest canonical lockfile path.</summary>
    [ObservableProperty]
    [NotifyPropertyChangedFor(nameof(HasPackValidation))]
    public partial string PackLockfilePath { get; set; } = string.Empty;

    /// <summary>Gets or sets the pack workflow error.</summary>
    [ObservableProperty]
    [NotifyPropertyChangedFor(nameof(HasPackError))]
    public partial string PackError { get; set; } = string.Empty;

    /// <summary>Gets or sets whether a pack operation is running.</summary>
    [ObservableProperty]
    public partial bool IsPackBusy { get; set; }

    /// <summary>Gets a value indicating whether the latest validation succeeded.</summary>
    public bool HasPackValidation => !string.IsNullOrEmpty(this.PackLockfilePath);

    /// <summary>Gets a value indicating whether the pack workflow has an error.</summary>
    public bool HasPackError => !string.IsNullOrEmpty(this.PackError);

    partial void OnSelectedPackConfigChanged(PackConfigItem? value)
    {
        if (value is null || this.SelectedInstance is null || this.SelectedProfile is null)
        {
            return;
        }

        _ = this.LoadPackConfigAsync(this.SelectedInstance, this.SelectedProfile, value.Path);
    }

    [RelayCommand]
    private void NewPackConfig()
    {
        this.SelectedPackConfig = null;
        this.PackConfigPath = "config/";
        this.PackConfigContent = string.Empty;
        this.PackError = string.Empty;
    }

    [RelayCommand]
    private async Task SavePackConfigAsync()
    {
        if (!this.IsPackConfigurationSupported || this.SelectedInstance is null || this.SelectedProfile is null || string.IsNullOrWhiteSpace(this.PackConfigPath) || this.IsPackBusy)
        {
            return;
        }

        string instance = this.SelectedInstance;
        string profile = this.SelectedProfile;
        string path = this.PackConfigPath.Trim();
        await this.RunPackOperationAsync(
            ["--format", "json", "pack", "config", "set", instance, path, "--content", this.PackConfigContent, "--profile", profile],
            $"Saved {path}.").ConfigureAwait(true);
        await this.LoadPackConfigsAsync(instance, profile).ConfigureAwait(true);
        this.SelectedPackConfig = this.PackConfigs.FirstOrDefault(config => string.Equals(config.Path, path, StringComparison.Ordinal));
    }

    [RelayCommand]
    private async Task RemovePackConfigAsync()
    {
        if (!this.IsPackConfigurationSupported || this.SelectedInstance is null || this.SelectedProfile is null || this.SelectedPackConfig is null || this.IsPackBusy)
        {
            return;
        }

        string instance = this.SelectedInstance;
        string profile = this.SelectedProfile;
        string path = this.SelectedPackConfig.Path;
        await this.RunPackOperationAsync(
            ["--format", "json", "pack", "config", "remove", instance, path, "--profile", profile],
            $"Removed {path}.").ConfigureAwait(true);
        await this.LoadPackConfigsAsync(instance, profile).ConfigureAwait(true);
        this.NewPackConfig();
    }

    [RelayCommand]
    private async Task ValidatePackAsync()
    {
        if (!this.IsPackConfigurationSupported || this.SelectedInstance is null || this.SelectedProfile is null || this.IsPackBusy)
        {
            return;
        }

        this.IsPackBusy = true;
        this.PackError = string.Empty;
        this.PackLockfilePath = string.Empty;
        try
        {
            CommandResult result = await this.client.RunCommandAsync(
                ["--format", "json", "pack", "validate", this.SelectedInstance, "--profile", this.SelectedProfile],
                CancellationToken.None).ConfigureAwait(true);
            if (result.ExitCode != 0)
            {
                throw new InvalidOperationException(result.StandardError.Trim());
            }

            using JsonDocument document = JsonDocument.Parse(result.StandardOutput);
            int files = document.RootElement.GetProperty("files").GetInt32();
            this.PackLockfilePath = document.RootElement.GetProperty("lockfile").GetString() ?? string.Empty;
            this.StatusMessage = $"Pack is valid: {files} reproducible file(s).";
        }
        catch (Exception exception) when (exception is IOException or SocketException or JsonException or InvalidOperationException)
        {
            this.PackError = exception.Message;
            this.StatusMessage = "Pack validation failed.";
        }
        finally
        {
            this.IsPackBusy = false;
        }
    }

    private void ClearPackState()
    {
        this.PackConfigs.Clear();
        this.SelectedPackConfig = null;
        this.PackConfigPath = string.Empty;
        this.PackConfigContent = string.Empty;
        this.PackLockfilePath = string.Empty;
        this.PackError = string.Empty;
        this.ClearExportPreview();
        this.ClearImportPreview();
        this.ClearCapturePreview();
    }

    private void SetDefaultPackOutputPath(string instance, string profile)
    {
        string extension = this.SelectedExportCodec?.Extensions is [string first, ..] ? first : "pack";
        string fileName = $"{instance}-{profile}.{extension}";
        this.PackOutputPath = Path.Combine(Environment.GetFolderPath(Environment.SpecialFolder.UserProfile), fileName);
    }

    private async Task LoadPackConfigsAsync(string instance, string profile)
    {
        this.PackError = string.Empty;
        this.PackConfigs.Clear();
        try
        {
            CommandResult result = await this.client.RunCommandAsync(
                ["--format", "json", "pack", "config", "list", instance, "--profile", profile],
                CancellationToken.None).ConfigureAwait(true);
            if (result.ExitCode != 0)
            {
                throw new InvalidOperationException(result.StandardError.Trim());
            }

            using JsonDocument document = JsonDocument.Parse(result.StandardOutput);
            if (!string.Equals(this.SelectedInstance, instance, StringComparison.Ordinal) || !string.Equals(this.SelectedProfile, profile, StringComparison.Ordinal))
            {
                return;
            }

            foreach (JsonElement config in document.RootElement.EnumerateArray())
            {
                this.PackConfigs.Add(new PackConfigItem(
                    config.GetProperty("path").GetString() ?? string.Empty,
                    config.GetProperty("digest").GetString() ?? string.Empty));
            }
        }
        catch (Exception exception) when (exception is IOException or SocketException or JsonException or InvalidOperationException)
        {
            this.PackError = exception.Message;
        }
    }

    private async Task LoadPackConfigAsync(string instance, string profile, string path)
    {
        try
        {
            CommandResult result = await this.client.RunCommandAsync(
                ["--format", "json", "pack", "config", "show", instance, path, "--profile", profile],
                CancellationToken.None).ConfigureAwait(true);
            if (result.ExitCode != 0)
            {
                throw new InvalidOperationException(result.StandardError.Trim());
            }

            using JsonDocument document = JsonDocument.Parse(result.StandardOutput);
            if (this.SelectedPackConfig is null || !string.Equals(this.SelectedPackConfig.Path, path, StringComparison.Ordinal))
            {
                return;
            }

            this.PackConfigPath = document.RootElement.GetProperty("path").GetString() ?? path;
            this.PackConfigContent = document.RootElement.GetProperty("content").GetString() ?? string.Empty;
            this.PackError = string.Empty;
        }
        catch (Exception exception) when (exception is IOException or SocketException or JsonException or InvalidOperationException)
        {
            this.PackError = exception.Message;
        }
    }

    private async Task RunPackOperationAsync(IReadOnlyList<string> arguments, string successMessage)
    {
        this.IsPackBusy = true;
        this.PackError = string.Empty;
        this.PackLockfilePath = string.Empty;
        try
        {
            CommandResult result = await this.client.RunCommandAsync(arguments, CancellationToken.None).ConfigureAwait(true);
            if (result.ExitCode != 0)
            {
                throw new InvalidOperationException(result.StandardError.Trim());
            }

            this.StatusMessage = successMessage;
        }
        catch (Exception exception) when (exception is IOException or SocketException or JsonException or InvalidOperationException)
        {
            this.PackError = exception.Message;
        }
        finally
        {
            this.IsPackBusy = false;
        }
    }
}
