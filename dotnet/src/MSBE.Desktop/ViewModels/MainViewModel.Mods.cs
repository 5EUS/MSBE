using System.Collections.ObjectModel;
using System.Net.Sockets;
using System.Text.Json;

using CommunityToolkit.Mvvm.ComponentModel;

using MSBE.Client;

namespace MSBE.Desktop.ViewModels;

/// <content>Profiles and mods for the selected instance.</content>
internal sealed partial class MainViewModel
{
    /// <summary>Gets the profiles belonging to the selected instance.</summary>
    public ObservableCollection<string> Profiles { get; } = [];

    /// <summary>Gets the mods in the selected profile, in application order.</summary>
    public ObservableCollection<ModListItem> Mods { get; } = [];

    /// <summary>Gets or sets the selected profile.</summary>
    [ObservableProperty]
    public partial string? SelectedProfile { get; set; }

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

    private static ModListItem ParseMod(string name, JsonElement entry)
    {
        string origin = entry.GetProperty("origin").GetString() ?? name;
        int fileCount = entry.GetProperty("files").GetArrayLength();
        if (entry.TryGetProperty("provider", out JsonElement provider) && provider.ValueKind == JsonValueKind.Object)
        {
            string source = provider.GetProperty("provider").GetString() ?? "Provider";
            string version = provider.GetProperty("version_number").GetString() ?? string.Empty;
            return new ModListItem(name, origin, source, version, fileCount);
        }

        return new ModListItem(name, origin, "Local file", string.Empty, fileCount);
    }

    partial void OnSelectedProfileChanged(string? value)
    {
        if (value is null || this.SelectedInstance is null)
        {
            this.Mods.Clear();
            this.OnPropertyChanged(nameof(this.IsModsEmpty));
            return;
        }

        _ = this.LoadModsAsync(this.SelectedInstance, value);
    }

    private void ClearProfilesAndMods()
    {
        this.SelectedProfile = null;
        this.Profiles.Clear();
        this.Mods.Clear();
        this.ModsError = string.Empty;
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
}
