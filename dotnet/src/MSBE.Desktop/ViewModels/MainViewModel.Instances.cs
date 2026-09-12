using System.Collections.ObjectModel;
using System.Net.Sockets;
using System.Text.Json;

using CommunityToolkit.Mvvm.ComponentModel;
using CommunityToolkit.Mvvm.Input;

using MSBE.Client;

namespace MSBE.Desktop.ViewModels;

/// <content>Instance discovery and selection.</content>
internal sealed partial class MainViewModel
{
    private readonly List<string> allInstances = [];

    /// <summary>Gets the registered game instances visible under the current filter.</summary>
    public ObservableCollection<string> Instances { get; } = [];

    /// <summary>Gets or sets the currently selected instance, if any.</summary>
    [ObservableProperty]
    public partial string? SelectedInstance { get; set; }

    /// <summary>Gets or sets whether registered instances are loading.</summary>
    [ObservableProperty]
    [NotifyPropertyChangedFor(nameof(IsInstanceListEmpty))]
    public partial bool IsInstancesLoading { get; set; }

    /// <summary>Gets or sets the last instance-loading error.</summary>
    [ObservableProperty]
    [NotifyPropertyChangedFor(nameof(HasInstanceError))]
    [NotifyPropertyChangedFor(nameof(IsInstanceListEmpty))]
    public partial string InstanceError { get; set; } = string.Empty;

    /// <summary>Gets or sets the selected instance's installation path.</summary>
    [ObservableProperty]
    public partial string SelectedInstanceRoot { get; set; } = string.Empty;

    /// <summary>Gets or sets the selected instance's plan description.</summary>
    [ObservableProperty]
    public partial string SelectedInstancePlan { get; set; } = string.Empty;

    /// <summary>Gets or sets the selected instance's game and loader target.</summary>
    [ObservableProperty]
    public partial string SelectedInstanceTarget { get; set; } = string.Empty;

    /// <summary>Gets or sets the selected instance's deployment summary.</summary>
    [ObservableProperty]
    public partial string SelectedInstanceDeployment { get; set; } = string.Empty;

    /// <summary>Gets a value indicating whether an instance is selected.</summary>
    public bool HasSelectedInstance => this.SelectedInstance is not null;

    /// <summary>Gets a value indicating whether the instance list is empty and idle.</summary>
    public bool IsInstanceListEmpty => !this.IsInstancesLoading && !this.HasInstanceError && this.Instances.Count == 0;

    /// <summary>Gets a value indicating whether instance loading has failed.</summary>
    public bool HasInstanceError => !string.IsNullOrEmpty(this.InstanceError);

    partial void OnInstanceSearchTextChanged(string value) => this.ApplyInstanceFilter();

    partial void OnInstanceSortChanged(string value) => this.ApplyInstanceFilter();

    partial void OnSelectedInstanceChanged(string? value)
    {
        this.OnPropertyChanged(nameof(this.HasSelectedInstance));
        if (value is null)
        {
            this.ClearInstanceDetails();
            this.ClearProfilesAndMods();
            return;
        }

        _ = this.LoadSelectedInstanceAsync(value);
        _ = this.LoadProfilesAsync(value);
    }

    /// <summary>Reloads registered instances from the daemon.</summary>
    /// <returns>A task that completes when the list has been refreshed.</returns>
    [RelayCommand]
    private async Task RefreshInstancesAsync()
    {
        this.IsInstancesLoading = true;
        this.InstanceError = string.Empty;
        this.StatusMessage = "Loading registered instances...";
        try
        {
            CommandResult result = await this.client.RunCommandAsync(["--format", "json", "instance", "list"], CancellationToken.None).ConfigureAwait(true);
            if (result.ExitCode != 0)
            {
                throw new InvalidOperationException(result.StandardError.Trim());
            }

            using JsonDocument document = JsonDocument.Parse(result.StandardOutput);
            this.allInstances.Clear();
            foreach (JsonElement item in document.RootElement.EnumerateArray())
            {
                string? name = item.GetString();
                if (!string.IsNullOrWhiteSpace(name))
                {
                    this.allInstances.Add(name);
                }
            }

            this.ApplyInstanceFilter();
            this.SelectedInstance ??= this.Instances.FirstOrDefault();
            this.StatusMessage = $"{this.allInstances.Count} registered instance(s).";
        }
        catch (Exception exception) when (exception is IOException or SocketException or JsonException or InvalidOperationException)
        {
            this.allInstances.Clear();
            this.Instances.Clear();
            this.InstanceError = exception.Message;
            this.StatusMessage = "Could not load instances.";
        }
        finally
        {
            this.IsInstancesLoading = false;
            this.OnPropertyChanged(nameof(this.IsInstanceListEmpty));
        }
    }

    private void ApplyInstanceFilter()
    {
        IEnumerable<string> filtered = this.allInstances.Where(name => name.Contains(this.InstanceSearchText, StringComparison.OrdinalIgnoreCase));
        filtered = string.Equals(this.InstanceSort, "Name (Z-A)", StringComparison.Ordinal)
            ? filtered.OrderByDescending(name => name, StringComparer.OrdinalIgnoreCase)
            : filtered.OrderBy(name => name, StringComparer.OrdinalIgnoreCase);

        string? selected = this.SelectedInstance;
        this.Instances.Clear();
        foreach (string instance in filtered)
        {
            this.Instances.Add(instance);
        }

        this.OnPropertyChanged(nameof(this.IsInstanceListEmpty));
        if (selected is not null && !this.Instances.Contains(selected, StringComparer.OrdinalIgnoreCase))
        {
            this.SelectedInstance = null;
        }
    }

    private void ClearInstanceDetails()
    {
        this.SelectedInstanceRoot = string.Empty;
        this.SelectedInstancePlan = string.Empty;
        this.SelectedInstanceTarget = string.Empty;
        this.SelectedInstanceDeployment = string.Empty;
    }

    private async Task LoadSelectedInstanceAsync(string name)
    {
        try
        {
            CommandResult result = await this.client.RunCommandAsync(["--format", "json", "status", name], CancellationToken.None).ConfigureAwait(true);
            if (result.ExitCode != 0)
            {
                throw new InvalidOperationException(result.StandardError.Trim());
            }

            using JsonDocument document = JsonDocument.Parse(result.StandardOutput);
            JsonElement status = document.RootElement;
            if (!string.Equals(this.SelectedInstance, name, StringComparison.Ordinal))
            {
                return;
            }

            this.SelectedInstanceRoot = status.GetProperty("root").GetString() ?? string.Empty;
            this.SelectedInstancePlan = $"{status.GetProperty("plan_id").GetString()} {status.GetProperty("plan_version").GetString()}";
            string gameVersion = status.TryGetProperty("game_version", out JsonElement version) && version.ValueKind == JsonValueKind.String
                ? version.GetString() ?? "Version not set"
                : "Version not set";
            this.SelectedInstanceTarget = $"{gameVersion} · {status.GetProperty("loader").GetString()}";
            string deployed = status.TryGetProperty("deployed_profile", out JsonElement profile) && profile.ValueKind == JsonValueKind.String
                ? profile.GetString() ?? "Not deployed"
                : "Not deployed";
            this.SelectedInstanceDeployment = $"{deployed} · {status.GetProperty("deployed_files").GetInt32()} managed files";
        }
        catch (Exception exception) when (exception is IOException or SocketException or JsonException or InvalidOperationException)
        {
            if (string.Equals(this.SelectedInstance, name, StringComparison.Ordinal))
            {
                this.ClearInstanceDetails();
                this.InstanceError = $"Could not load {name}: {exception.Message}";
            }
        }
    }
}
