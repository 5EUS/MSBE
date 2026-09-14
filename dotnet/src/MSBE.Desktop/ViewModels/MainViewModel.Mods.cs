using System.Collections.ObjectModel;
using System.Net.Sockets;
using System.Text.Json;

using CommunityToolkit.Mvvm.ComponentModel;

using MSBE.Client;
using MSBE.Desktop.Resources;

namespace MSBE.Desktop.ViewModels;

/// <content>Profiles and mods for the selected instance.</content>
internal sealed partial class MainViewModel
{
    /// <summary>Gets the profiles belonging to the selected instance.</summary>
    public ObservableCollection<string> Profiles { get; } = [];

    /// <summary>Gets the mods in the selected profile, in application order.</summary>
    public ObservableCollection<ModListItem> Mods { get; } = [];

    /// <summary>Gets or sets the mod selected for profile actions.</summary>
    [ObservableProperty]
    public partial ModListItem? SelectedMod { get; set; }

    /// <summary>Gets or sets the selected profile.</summary>
    [ObservableProperty]
    [NotifyPropertyChangedFor(nameof(IsSelectedProfileDeployed))]
    [NotifyPropertyChangedFor(nameof(CanRemoveSelectedProfile))]
    public partial string? SelectedProfile { get; set; }

    /// <summary>Gets or sets the profile currently deployed to disk.</summary>
    [ObservableProperty]
    [NotifyPropertyChangedFor(nameof(IsSelectedProfileDeployed))]
    [NotifyPropertyChangedFor(nameof(CanRemoveSelectedProfile))]
    [NotifyPropertyChangedFor(nameof(CanRollbackLatest))]
    public partial string? DeployedProfile { get; set; }

    /// <summary>Gets or sets whether profile mods are loading.</summary>
    [ObservableProperty]
    [NotifyPropertyChangedFor(nameof(IsModsEmpty))]
    public partial bool IsModsLoading { get; set; }

    /// <summary>Gets or sets the current mod-list error.</summary>
    [ObservableProperty]
    [NotifyPropertyChangedFor(nameof(HasModsError))]
    [NotifyPropertyChangedFor(nameof(IsModsEmpty))]
    public partial string ModsError { get; set; } = string.Empty;

    /// <summary>Gets a value indicating whether loading the mod list failed.</summary>
    public bool HasModsError => !string.IsNullOrEmpty(this.ModsError);

    /// <summary>Gets a value indicating whether the selected profile has no mods.</summary>
    public bool IsModsEmpty => !this.IsModsLoading && !this.HasModsError && this.Mods.Count == 0;

    /// <summary>Gets a value indicating whether the selected profile is currently deployed.</summary>
    public bool IsSelectedProfileDeployed => this.SelectedProfile is not null && string.Equals(this.SelectedProfile, this.DeployedProfile, StringComparison.Ordinal);

    /// <summary>Gets a value indicating whether the selected profile can be deleted.</summary>
    public bool CanRemoveSelectedProfile => this.SelectedProfile is not null && !this.IsSelectedProfileDeployed && !this.IsProfileMutationBusy;

    /// <summary>Gets a value indicating whether a deployment is available to roll back.</summary>
    public bool CanRollbackLatest => this.DeployedProfile is not null && !this.IsRollbackBusy;

    private static ModListItem ParseMod(string name, JsonElement entry)
    {
        string origin = entry.GetProperty("origin").GetString() ?? name;
        int fileCount = entry.GetProperty("files").GetArrayLength();
        if (entry.TryGetProperty("provider", out JsonElement provider) && provider.ValueKind == JsonValueKind.Object)
        {
            string source = provider.GetProperty("provider").GetString() ?? "Provider";
            string project = provider.GetProperty("project").GetString() ?? string.Empty;
            string version = provider.GetProperty("version_number").GetString() ?? string.Empty;
            return new ModListItem(name, origin, source, project, version, fileCount);
        }

        return new ModListItem(name, origin, Strings.ModSourceLocalFile, string.Empty, string.Empty, fileCount);
    }

    partial void OnSelectedProfileChanged(string? value)
    {
        this.ClearBrowseResultsForTargetChange();
        this.ClearPackState();
        this.ClearProfileHistory();
        if (value is null || this.SelectedInstance is null)
        {
            this.Mods.Clear();
            this.ClearProfileTarget();
            this.OnPropertyChanged(nameof(this.IsModsEmpty));
            return;
        }

        this.SetDefaultPackOutputPath(this.SelectedInstance, value);
        _ = this.LoadModsAsync(this.SelectedInstance, value);
        if (this.IsPackConfigurationSupported)
        {
            _ = this.LoadPackConfigsAsync(this.SelectedInstance, value);
        }
        else
        {
            this.PackError = Strings.PackDaemonOutdated;
        }

        if (this.IsHistoryWorkspace)
        {
            _ = this.LoadHistoryAsync();
        }
    }

    private void ClearProfilesAndMods()
    {
        this.SelectedProfile = null;
        this.DeployedProfile = null;
        this.Profiles.Clear();
        this.Mods.Clear();
        this.SelectedMod = null;
        this.ModsError = string.Empty;
        this.ClearProfileTarget();
        this.OnPropertyChanged(nameof(this.IsModsEmpty));
    }

    private async Task LoadProfilesAsync(string instance)
    {
        this.ClearProfilesAndMods();
        this.IsModsLoading = true;
        try
        {
            CommandResult result = await this.client.RunCommandAsync(["--format", "json", "profile", "list", instance], CancellationToken.None).ConfigureAwait(true);
            if (result.ExitCode != 0)
            {
                throw new InvalidOperationException(result.StandardError.Trim());
            }

            using JsonDocument document = JsonDocument.Parse(result.StandardOutput);
            if (!string.Equals(this.SelectedInstance, instance, StringComparison.Ordinal))
            {
                return;
            }

            foreach (JsonElement profile in document.RootElement.GetProperty("profiles").EnumerateArray())
            {
                string? name = profile.GetString();
                if (!string.IsNullOrWhiteSpace(name))
                {
                    this.Profiles.Add(name);
                }
            }

            string? deployed = document.RootElement.TryGetProperty("deployed", out JsonElement deployedProfile) && deployedProfile.ValueKind == JsonValueKind.String
                ? deployedProfile.GetString()
                : null;
            this.DeployedProfile = deployed;
            this.SelectedProfile = deployed is not null && this.Profiles.Contains(deployed, StringComparer.Ordinal)
                ? deployed
                : this.Profiles.FirstOrDefault();
        }
        catch (Exception exception) when (exception is IOException or SocketException or JsonException or InvalidOperationException)
        {
            if (string.Equals(this.SelectedInstance, instance, StringComparison.Ordinal))
            {
                this.ModsError = exception.Message;
            }
        }
        finally
        {
            this.IsModsLoading = false;
            this.OnPropertyChanged(nameof(this.IsModsEmpty));
        }
    }

    private async Task LoadModsAsync(string instance, string profile)
    {
        this.IsModsLoading = true;
        this.ModsError = string.Empty;
        this.Mods.Clear();
        this.SelectedMod = null;
        try
        {
            CommandResult result = await this.client.RunCommandAsync(["--format", "json", "profile", "show", instance, profile], CancellationToken.None).ConfigureAwait(true);
            if (result.ExitCode != 0)
            {
                throw new InvalidOperationException(result.StandardError.Trim());
            }

            using JsonDocument document = JsonDocument.Parse(result.StandardOutput);
            if (!string.Equals(this.SelectedInstance, instance, StringComparison.Ordinal) || !string.Equals(this.SelectedProfile, profile, StringComparison.Ordinal))
            {
                return;
            }

            this.LoadProfileTarget(document.RootElement);
            JsonElement mods = document.RootElement.GetProperty("mods");
            var seen = new HashSet<string>(StringComparer.Ordinal);
            if (document.RootElement.TryGetProperty("order", out JsonElement order))
            {
                foreach (JsonElement orderedName in order.EnumerateArray())
                {
                    string? name = orderedName.GetString();
                    if (name is not null && seen.Add(name) && mods.TryGetProperty(name, out JsonElement entry))
                    {
                        this.Mods.Add(ParseMod(name, entry));
                    }
                }
            }

            foreach (JsonProperty mod in mods.EnumerateObject().Where(mod => !seen.Contains(mod.Name)))
            {
                seen.Add(mod.Name);
                this.Mods.Add(ParseMod(mod.Name, mod.Value));
            }
        }
        catch (Exception exception) when (exception is IOException or SocketException or JsonException or InvalidOperationException)
        {
            if (string.Equals(this.SelectedInstance, instance, StringComparison.Ordinal) && string.Equals(this.SelectedProfile, profile, StringComparison.Ordinal))
            {
                this.ModsError = exception.Message;
            }
        }
        finally
        {
            this.IsModsLoading = false;
            this.OnPropertyChanged(nameof(this.IsModsEmpty));
        }
    }

    private void LoadProfileTarget(JsonElement profile)
    {
        if (!profile.TryGetProperty("target", out JsonElement target) || target.ValueKind != JsonValueKind.Object)
        {
            this.ClearProfileTarget();
            return;
        }

        this.SelectedProfileLoader = target.GetProperty("loader").GetString() ?? string.Empty;
        this.SelectedProfileLoaderVersion = target.TryGetProperty("loader_version", out JsonElement loaderVersion) && loaderVersion.ValueKind == JsonValueKind.String
            ? loaderVersion.GetString() ?? string.Empty
            : string.Empty;
        string side = target.GetProperty("side").GetString() ?? "client";
        this.SelectedProfileSide = string.Equals(side, "server", StringComparison.OrdinalIgnoreCase) ? "Server" : "Client";
    }

    private void ClearProfileTarget()
    {
        this.SelectedProfileLoader = string.Empty;
        this.SelectedProfileLoaderVersion = string.Empty;
        this.SelectedProfileSide = "Client";
    }

    private void ClearBrowseResultsForTargetChange()
    {
        this.BrowseResults.Clear();
        this.SelectedBrowseResult = null;
        this.BrowseError = string.Empty;
        this.OnPropertyChanged(nameof(this.IsBrowseEmpty));
    }
}
