using System.Collections.ObjectModel;
using System.Collections.Specialized;
using System.ComponentModel;
using System.Net.Sockets;
using System.Text.Json;

using CommunityToolkit.Mvvm.ComponentModel;
using CommunityToolkit.Mvvm.Input;

using MSBE.Client;
using MSBE.Desktop.Resources;

namespace MSBE.Desktop.ViewModels;

/// <content>Provider search and profile installation.</content>
internal sealed partial class MainViewModel
{
    private readonly HashSet<BrowseResultItem> observedBrowseResults = [];

    /// <summary>Gets compatible provider results.</summary>
    public ObservableCollection<BrowseResultItem> BrowseResults { get; } = [];

    /// <summary>Gets provider results marked for bulk installation.</summary>
    public ObservableCollection<BrowseResultItem> MarkedBrowseResults { get; } = [];

    /// <summary>Gets or sets the provider search query.</summary>
    [ObservableProperty]
    public partial string BrowseQuery { get; set; } = string.Empty;

    /// <summary>Gets or sets the selected provider result.</summary>
    [ObservableProperty]
    public partial BrowseResultItem? SelectedBrowseResult { get; set; }

    /// <summary>Gets or sets whether provider search is running.</summary>
    [ObservableProperty]
    [NotifyPropertyChangedFor(nameof(IsBrowseEmpty))]
    public partial bool IsBrowseLoading { get; set; }

    /// <summary>Gets or sets whether required dependencies are included.</summary>
    [ObservableProperty]
    public partial bool BrowseWithDependencies { get; set; } = true;

    /// <summary>Gets or sets the current provider error.</summary>
    [ObservableProperty]
    [NotifyPropertyChangedFor(nameof(HasBrowseError))]
    [NotifyPropertyChangedFor(nameof(IsBrowseEmpty))]
    public partial string BrowseError { get; set; } = string.Empty;

    /// <summary>Gets a value indicating whether provider search failed.</summary>
    public bool HasBrowseError => !string.IsNullOrEmpty(this.BrowseError);

    /// <summary>Gets a value indicating whether search has no results.</summary>
    public bool IsBrowseEmpty => !this.IsBrowseLoading && !this.HasBrowseError && this.BrowseResults.Count == 0;

    /// <summary>Gets a value indicating whether any results are marked for installation.</summary>
    public bool HasMarkedBrowseResults => this.MarkedBrowseResults.Count > 0;

    /// <summary>Gets the number of results marked for installation.</summary>
    public string MarkedBrowseResultCount => Strings.FormatBrowseMarkedCount(this.MarkedBrowseResults.Count);

    /// <summary>Gets a value indicating whether any marked result waits for the user once it is queued.</summary>
    public bool HasMarkedNeedingUser => this.MarkedBrowseResults.Any(result => result.NeedsUser);

    /// <summary>Gets how many marked results wait for the user once they are queued.</summary>
    public string MarkedNeedUserText => Strings.FormatBrowseMarkedNeedUser(this.MarkedBrowseResults.Count(result => result.NeedsUser));

    /// <summary>Gets a value indicating whether the marked results can be installed.</summary>
    public bool CanInstallMarkedBrowseResults => this.HasMarkedBrowseResults;

    [RelayCommand]
    private async Task SearchBrowseAsync()
    {
        if (this.SelectedInstance is null || this.SelectedProfile is null || string.IsNullOrWhiteSpace(this.BrowseQuery))
        {
            return;
        }

        string instance = this.SelectedInstance;
        string profile = this.SelectedProfile;
        this.IsBrowseLoading = true;
        this.BrowseError = string.Empty;
        this.BrowseResults.Clear();
        try
        {
            await this.LoadModsAsync(instance, profile).ConfigureAwait(true);
            if (!string.Equals(this.SelectedInstance, instance, StringComparison.Ordinal) ||
                !string.Equals(this.SelectedProfile, profile, StringComparison.Ordinal))
            {
                return;
            }

            CommandResult result = await this.client.RunCommandAsync(
                ["--format", "json", "search", instance, this.BrowseQuery.Trim(), "--profile", profile, "--limit", "30"],
                CancellationToken.None).ConfigureAwait(true);
            if (result.ExitCode != 0)
            {
                throw new InvalidOperationException(result.StandardError.Trim());
            }

            using JsonDocument document = JsonDocument.Parse(result.StandardOutput);
            foreach (JsonElement hit in document.RootElement.EnumerateArray())
            {
                this.BrowseResults.Add(this.BrowseResultFrom(hit));
            }
        }
        catch (Exception exception) when (exception is IOException or SocketException or JsonException or InvalidOperationException)
        {
            this.BrowseError = exception.Message;
        }
        finally
        {
            this.IsBrowseLoading = false;
            this.OnPropertyChanged(nameof(this.IsBrowseEmpty));
        }
    }

    [RelayCommand]
    private async Task AddBrowseResultAsync()
    {
        if (this.SelectedInstance is not { } instance || this.SelectedProfile is not { } profile || !this.CanInstallMarkedBrowseResults)
        {
            return;
        }

        if (!this.IsDownloadQueueSupported)
        {
            this.StatusMessage = Strings.DownloadsUnsupported;
            return;
        }

        BrowseResultItem[] marked = [.. this.MarkedBrowseResults];
        HashSet<long> queued = [];
        try
        {
            foreach (BrowseResultItem resultItem in marked)
            {
                DownloadInfo download = await this.client.EnqueueDownloadAsync(
                    instance,
                    profile,
                    resultItem.Source,
                    this.BrowseWithDependencies,
                    resultItem.Title,
                    CancellationToken.None).ConfigureAwait(true);
                if (resultItem.IconSource is { } icon)
                {
                    this.downloadIcons.TryAdd(download.Id, icon);
                }

                if (!this.downloads.ContainsKey(download.Id))
                {
                    queued.Add(download.Id);
                }

                resultItem.IsMarked = false;
            }
        }
        catch (Exception exception) when (exception is IOException or SocketException or JsonException or InvalidOperationException or KeyNotFoundException)
        {
            this.StatusMessage = Strings.FormatBrowseQueueFailed(profile, exception.Message);
            await this.RefreshDownloadsAsync().ConfigureAwait(true);
            return;
        }

        await this.RefreshDownloadsAsync().ConfigureAwait(true);
        this.StatusMessage = queued.Count == marked.Length
            ? Strings.FormatBrowseQueued(queued.Count, profile)
            : Strings.FormatBrowseQueuedSomeAlready(queued.Count, profile, marked.Length - queued.Count);
    }

    /// <summary>A search hit as Browse shows it: an HTTPS icon only, and what the user must do for it.</summary>
    private BrowseResultItem BrowseResultFrom(JsonElement hit)
    {
        string? iconUrl = null;
        if (hit.TryGetProperty("icon_url", out JsonElement icon) &&
            icon.ValueKind == JsonValueKind.String &&
            Uri.TryCreate(icon.GetString(), UriKind.Absolute, out Uri? iconUri) &&
            string.Equals(iconUri.Scheme, Uri.UriSchemeHttps, StringComparison.OrdinalIgnoreCase))
        {
            iconUrl = iconUri.AbsoluteUri;
        }

        string provider = hit.GetProperty("provider").GetString() ?? string.Empty;
        return new BrowseResultItem(
            provider,
            hit.GetProperty("project").GetString() ?? string.Empty,
            hit.GetProperty("slug").GetString() ?? string.Empty,
            hit.GetProperty("title").GetString() ?? string.Empty,
            hit.GetProperty("description").GetString() ?? string.Empty,
            iconUrl,
            hit.GetProperty("downloads").GetUInt64(),
            this.IsBrowseResultInstalled(hit))
        {
            Attention = this.AttentionFor(provider),
        };
    }

    private void OnBrowseResultsChanged(object? sender, NotifyCollectionChangedEventArgs eventArgs)
    {
        if (eventArgs.Action == NotifyCollectionChangedAction.Reset)
        {
            foreach (BrowseResultItem resultItem in this.observedBrowseResults)
            {
                resultItem.PropertyChanged -= this.OnBrowseResultPropertyChanged;
            }

            this.observedBrowseResults.Clear();
            this.MarkedBrowseResults.Clear();
            this.NotifyMarkedBrowseResultsChanged();
            return;
        }

        if (eventArgs.OldItems is not null)
        {
            foreach (BrowseResultItem resultItem in eventArgs.OldItems)
            {
                resultItem.PropertyChanged -= this.OnBrowseResultPropertyChanged;
                this.observedBrowseResults.Remove(resultItem);
                this.MarkedBrowseResults.Remove(resultItem);
            }
        }

        if (eventArgs.NewItems is not null)
        {
            foreach (BrowseResultItem resultItem in eventArgs.NewItems)
            {
                resultItem.PropertyChanged += this.OnBrowseResultPropertyChanged;
                this.observedBrowseResults.Add(resultItem);
                if (resultItem.IsMarked && !resultItem.IsInstalled)
                {
                    this.MarkedBrowseResults.Add(resultItem);
                }
            }
        }

        this.NotifyMarkedBrowseResultsChanged();
    }

    private void OnBrowseResultPropertyChanged(object? sender, PropertyChangedEventArgs eventArgs)
    {
        if (sender is not BrowseResultItem resultItem || !string.Equals(eventArgs.PropertyName, nameof(BrowseResultItem.IsMarked), StringComparison.Ordinal))
        {
            return;
        }

        if (resultItem.IsMarked && !resultItem.IsInstalled)
        {
            if (!this.MarkedBrowseResults.Contains(resultItem))
            {
                this.MarkedBrowseResults.Add(resultItem);
            }
        }
        else
        {
            this.MarkedBrowseResults.Remove(resultItem);
        }

        this.NotifyMarkedBrowseResultsChanged();
    }

    private void NotifyMarkedBrowseResultsChanged()
    {
        this.OnPropertyChanged(nameof(this.HasMarkedBrowseResults));
        this.OnPropertyChanged(nameof(this.MarkedBrowseResultCount));
        this.OnPropertyChanged(nameof(this.CanInstallMarkedBrowseResults));
        this.OnPropertyChanged(nameof(this.HasMarkedNeedingUser));
        this.OnPropertyChanged(nameof(this.MarkedNeedUserText));
    }

    private bool IsBrowseResultInstalled(JsonElement hit)
    {
        string provider = hit.GetProperty("provider").GetString() ?? string.Empty;
        string project = hit.GetProperty("project").GetString() ?? string.Empty;
        string reference = hit.GetProperty("slug").GetString() ?? string.Empty;
        return this.Mods.Any(mod =>
            string.Equals(mod.Source, provider, StringComparison.Ordinal) &&
            (string.Equals(mod.Project, project, StringComparison.Ordinal) ||
             (string.IsNullOrEmpty(mod.Project) && string.Equals(mod.Name, reference, StringComparison.Ordinal))));
    }
}
