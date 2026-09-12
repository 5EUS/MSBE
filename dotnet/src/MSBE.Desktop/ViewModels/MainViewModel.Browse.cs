using System.Collections.ObjectModel;
using System.Net.Sockets;
using System.Text.Json;

using CommunityToolkit.Mvvm.ComponentModel;
using CommunityToolkit.Mvvm.Input;

using MSBE.Client;

namespace MSBE.Desktop.ViewModels;

/// <content>Provider search and profile installation.</content>
internal sealed partial class MainViewModel
{
    /// <summary>Gets compatible provider results.</summary>
    public ObservableCollection<BrowseResultItem> BrowseResults { get; } = [];

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

    /// <summary>Gets or sets whether an installation is running.</summary>
    [ObservableProperty]
    public partial bool IsBrowseInstalling { get; set; }

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

    [RelayCommand]
    private async Task SearchBrowseAsync()
    {
        if (this.SelectedInstance is null || this.SelectedProfile is null || string.IsNullOrWhiteSpace(this.BrowseQuery))
        {
            return;
        }

        this.IsBrowseLoading = true;
        this.BrowseError = string.Empty;
        this.BrowseResults.Clear();
        try
        {
            CommandResult result = await this.client.RunCommandAsync(
                ["--format", "json", "search", this.SelectedInstance, this.BrowseQuery.Trim(), "--profile", this.SelectedProfile, "--limit", "30"],
                CancellationToken.None).ConfigureAwait(true);
            if (result.ExitCode != 0)
            {
                throw new InvalidOperationException(result.StandardError.Trim());
            }

            using JsonDocument document = JsonDocument.Parse(result.StandardOutput);
            foreach (JsonElement hit in document.RootElement.EnumerateArray())
            {
                string? iconUrl = null;
                if (hit.TryGetProperty("icon_url", out JsonElement icon) &&
                    icon.ValueKind == JsonValueKind.String &&
                    Uri.TryCreate(icon.GetString(), UriKind.Absolute, out Uri? iconUri) &&
                    string.Equals(iconUri.Scheme, Uri.UriSchemeHttps, StringComparison.OrdinalIgnoreCase))
                {
                    iconUrl = iconUri.AbsoluteUri;
                }

                this.BrowseResults.Add(new BrowseResultItem(
                    hit.GetProperty("provider").GetString() ?? string.Empty,
                    hit.GetProperty("slug").GetString() ?? string.Empty,
                    hit.GetProperty("title").GetString() ?? string.Empty,
                    hit.GetProperty("description").GetString() ?? string.Empty,
                    iconUrl,
                    hit.GetProperty("downloads").GetUInt64()));
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
        if (this.SelectedInstance is null || this.SelectedProfile is null || this.SelectedBrowseResult is null || this.IsBrowseInstalling)
        {
            return;
        }

        this.IsBrowseInstalling = true;
        this.BrowseError = string.Empty;
        try
        {
            BrowseResultItem selected = this.SelectedBrowseResult;
            List<string> arguments = ["--format", "json", "add", this.SelectedInstance, selected.Source, "--profile", this.SelectedProfile];
            if (this.BrowseWithDependencies)
            {
                arguments.Add("--with-deps");
            }

            CommandResult result = await this.client.RunCommandAsync(arguments, CancellationToken.None).ConfigureAwait(true);
            if (result.ExitCode != 0)
            {
                throw new InvalidOperationException(result.StandardError.Trim());
            }

            await this.LoadModsAsync(this.SelectedInstance, this.SelectedProfile).ConfigureAwait(true);
            this.StatusMessage = $"Added {selected.Title} to {this.SelectedProfile}. Review deployment to apply it.";
        }
        catch (Exception exception) when (exception is IOException or SocketException or InvalidOperationException)
        {
            this.BrowseError = exception.Message;
        }
        finally
        {
            this.IsBrowseInstalling = false;
        }
    }
}
