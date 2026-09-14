using System.Collections.ObjectModel;
using System.Net.Sockets;
using System.Text.Json;

using CommunityToolkit.Mvvm.ComponentModel;
using CommunityToolkit.Mvvm.Input;

using MSBE.Client;
using MSBE.Desktop.Resources;

namespace MSBE.Desktop.ViewModels;

/// <content>
/// What the daemon describes about each enabled provider. Browse uses it to say which results wait for
/// the user before they are queued, and to add projects from providers that cannot be searched.
/// </content>
internal sealed partial class MainViewModel
{
    private readonly Dictionary<string, ProviderInfo> providers = new(StringComparer.Ordinal);

    /// <summary>Gets the enabled providers that cannot be searched, whose projects are added by reference.</summary>
    public ObservableCollection<ProviderInfo> LinkProviders { get; } = [];

    /// <summary>Gets or sets the provider a project reference is added from.</summary>
    [ObservableProperty]
    [NotifyPropertyChangedFor(nameof(LinkReferencePlaceholder))]
    [NotifyPropertyChangedFor(nameof(CanAddByLink))]
    public partial ProviderInfo? SelectedLinkProvider { get; set; }

    /// <summary>Gets or sets the project reference to add.</summary>
    [ObservableProperty]
    [NotifyPropertyChangedFor(nameof(CanAddByLink))]
    public partial string LinkReference { get; set; } = string.Empty;

    /// <summary>Gets a value indicating whether any enabled provider is added by reference.</summary>
    public bool HasLinkProviders => this.LinkProviders.Count > 0;

    /// <summary>Gets the hint shown in the reference box.</summary>
    public string LinkReferencePlaceholder => this.SelectedLinkProvider is { Prefix: { } prefix }
        ? Strings.FormatBrowseLinkPlaceholder(prefix)
        : Strings.BrowseLinkPlaceholderNoProvider;

    /// <summary>Gets a value indicating whether a reference can be added.</summary>
    public bool CanAddByLink => this.SelectedLinkProvider is not null && this.LinkReference.Trim().Length > 0;

    [RelayCommand]
    private async Task LoadProvidersAsync()
    {
        try
        {
            IReadOnlyList<ProviderInfo> described = await this.client.ListProvidersAsync(CancellationToken.None).ConfigureAwait(true);
            this.providers.Clear();
            foreach (ProviderInfo provider in described)
            {
                this.providers[provider.Id] = provider;
            }

            string? selected = this.SelectedLinkProvider?.Id;
            this.LinkProviders.Clear();
            foreach (ProviderInfo provider in described.Where(provider => !provider.IsSearchable && provider.Prefix is not null))
            {
                this.LinkProviders.Add(provider);
            }

            this.SelectedLinkProvider = this.LinkProviders.FirstOrDefault(provider => string.Equals(provider.Id, selected, StringComparison.Ordinal))
                ?? this.LinkProviders.FirstOrDefault();
            this.OnPropertyChanged(nameof(this.HasLinkProviders));
            foreach (BrowseResultItem result in this.BrowseResults)
            {
                result.Attention = this.AttentionFor(result.Provider);
            }

            this.NotifyMarkedBrowseResultsChanged();
        }
        catch (MsbeRpcException exception) when (exception.Code == MethodNotFoundCode)
        {
            // A daemon that describes no providers marks no result and offers no reference box.
            this.providers.Clear();
            this.LinkProviders.Clear();
            this.OnPropertyChanged(nameof(this.HasLinkProviders));
        }
        catch (Exception exception) when (exception is IOException or SocketException or JsonException or InvalidOperationException or KeyNotFoundException)
        {
            this.StatusMessage = Strings.FormatProvidersFailed(exception.Message);
        }
    }

    /// <summary>What the user must do before or while a result from <paramref name="providerId" /> downloads, or empty when nothing.</summary>
    private string AttentionFor(string providerId)
    {
        if (!this.providers.TryGetValue(providerId, out ProviderInfo? provider))
        {
            return string.Empty;
        }

        if (provider.RequiresAuth && !provider.IsSignedIn)
        {
            return Strings.FormatBrowseNeedsSignIn(provider.Name);
        }

        if (provider.RequiresAcknowledgement && !provider.IsAcknowledged)
        {
            return Strings.FormatBrowseNeedsTerms(provider.Name);
        }

        return provider.Acquisition switch
        {
            "direct_https" => string.Empty,
            "external_tool" => Strings.FormatBrowseNeedsTool(provider.Name),
            _ => Strings.FormatBrowseNeedsPage(provider.Name),
        };
    }

    [RelayCommand]
    private async Task AddByLinkAsync()
    {
        if (this.SelectedInstance is not { } instance || this.SelectedProfile is not { } profile ||
            this.SelectedLinkProvider is not { Prefix: { } prefix } provider || !this.CanAddByLink)
        {
            return;
        }

        if (!this.IsDownloadQueueSupported)
        {
            this.StatusMessage = Strings.DownloadsUnsupported;
            return;
        }

        string reference = this.LinkReference.Trim();
        string source = reference.StartsWith(prefix, StringComparison.OrdinalIgnoreCase) ? reference : string.Concat(prefix, reference);
        try
        {
            DownloadInfo download = await this.client.EnqueueDownloadAsync(instance, profile, source, this.BrowseWithDependencies, null, CancellationToken.None).ConfigureAwait(true);
            this.LinkReference = string.Empty;
            this.StatusMessage = Strings.FormatBrowseAddedByLink(download.Title ?? source, provider.Name, profile);
        }
        catch (Exception exception) when (exception is IOException or SocketException or JsonException or InvalidOperationException or KeyNotFoundException)
        {
            this.StatusMessage = Strings.FormatBrowseAddByLinkFailed(exception.Message);
            return;
        }

        await this.RefreshDownloadsAsync().ConfigureAwait(true);
    }
}
