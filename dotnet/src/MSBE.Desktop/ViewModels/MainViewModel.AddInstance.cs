using System.Collections.ObjectModel;
using System.Net.Sockets;

using CommunityToolkit.Mvvm.ComponentModel;
using CommunityToolkit.Mvvm.Input;

using MSBE.Client;

namespace MSBE.Desktop.ViewModels;

/// <content>Native instance registration workflow.</content>
internal sealed partial class MainViewModel
{
    /// <summary>Gets supported games matching the wizard search.</summary>
    public ObservableCollection<GameInfo> FilteredGames { get; } = [];

    /// <summary>Gets or sets whether the add-instance window is open.</summary>
    [ObservableProperty]
    public partial bool IsAddInstanceOpen { get; set; }

    /// <summary>Gets or sets the new instance name.</summary>
    [ObservableProperty]
    public partial string NewInstanceName { get; set; } = string.Empty;

    /// <summary>Gets or sets the game installation directory.</summary>
    [ObservableProperty]
    public partial string NewInstanceRoot { get; set; } = string.Empty;

    /// <summary>Gets or sets the supported game being registered.</summary>
    [ObservableProperty]
    public partial GameInfo? NewInstanceGame { get; set; }

    /// <summary>Gets or sets text used to filter supported games.</summary>
    [ObservableProperty]
    [NotifyPropertyChangedFor(nameof(HasGameSearchText))]
    public partial string GameSearchText { get; set; } = string.Empty;

    /// <summary>Gets or sets the selected mod loading ecosystem.</summary>
    [ObservableProperty]
    public partial string NewInstanceLoader { get; set; } = string.Empty;

    /// <summary>Gets or sets the target side.</summary>
    [ObservableProperty]
    public partial string NewInstanceSide { get; set; } = "Client";

    /// <summary>Gets or sets the optional game version.</summary>
    [ObservableProperty]
    public partial string NewInstanceGameVersion { get; set; } = string.Empty;

    /// <summary>Gets or sets the registration error shown in the form.</summary>
    [ObservableProperty]
    [NotifyPropertyChangedFor(nameof(HasAddInstanceError))]
    public partial string AddInstanceError { get; set; } = string.Empty;

    /// <summary>Gets or sets whether registration is in progress.</summary>
    [ObservableProperty]
    public partial bool IsAddingInstance { get; set; }

    /// <summary>Gets the supported target sides.</summary>
    public IReadOnlyList<string> InstanceSides { get; } = ["Client", "Server"];

    /// <summary>Gets a value indicating whether the registration form has an error.</summary>
    public bool HasAddInstanceError => !string.IsNullOrEmpty(this.AddInstanceError);

    /// <summary>Gets a value indicating whether the game search has no matches.</summary>
    public bool IsGameSearchEmpty => this.FilteredGames.Count == 0;

    /// <summary>Gets a value indicating whether the game search is filtering.</summary>
    public bool HasGameSearchText => !string.IsNullOrWhiteSpace(this.GameSearchText);

    [RelayCommand]
    private void AddInstance()
    {
        this.AddInstanceError = string.Empty;
        this.GameSearchText = string.Empty;
        this.RefreshGameSearch();
        this.NewInstanceGame ??= this.Games.FirstOrDefault();
        this.IsAddInstanceOpen = true;
    }

    partial void OnGameSearchTextChanged(string value) => this.RefreshGameSearch();

    partial void OnNewInstanceGameChanged(GameInfo? value)
    {
        if (value is null)
        {
            this.NewInstanceLoader = string.Empty;
            return;
        }

        if (!value.Loaders.Contains(this.NewInstanceLoader, StringComparer.Ordinal))
        {
            this.NewInstanceLoader = value.Loaders.FirstOrDefault() ?? string.Empty;
        }
    }

    [RelayCommand]
    private void CancelAddInstance() => this.IsAddInstanceOpen = false;

    [RelayCommand]
    private async Task SubmitAddInstanceAsync()
    {
        if (this.IsAddingInstance)
        {
            return;
        }

        if (string.IsNullOrWhiteSpace(this.NewInstanceName) ||
            string.IsNullOrWhiteSpace(this.NewInstanceRoot) ||
            this.NewInstanceGame is null ||
            string.IsNullOrWhiteSpace(this.NewInstanceLoader))
        {
            this.AddInstanceError = "Name, game, game folder, and ecosystem are required.";
            return;
        }

        this.IsAddingInstance = true;
        this.AddInstanceError = string.Empty;
        try
        {
            List<string> arguments =
            [
                "--format", "json", "instance", "add", this.NewInstanceName.Trim(),
                "--root", this.NewInstanceRoot.Trim(),
                "--game", this.NewInstanceGame.Id,
                "--loader", this.NewInstanceLoader.Trim(),
                "--side", string.Equals(this.NewInstanceSide, "Server", StringComparison.Ordinal) ? "server" : "client",
            ];
            if (!string.IsNullOrWhiteSpace(this.NewInstanceGameVersion))
            {
                arguments.Add("--game-version");
                arguments.Add(this.NewInstanceGameVersion.Trim());
            }

            string instanceName = this.NewInstanceName.Trim();
            CommandResult result = await this.client.RunCommandAsync(arguments, CancellationToken.None).ConfigureAwait(true);
            if (result.ExitCode != 0)
            {
                throw new InvalidOperationException(result.StandardError.Trim());
            }

            await this.RefreshInstancesAsync().ConfigureAwait(true);
            this.SelectedInstance = instanceName;
            this.IsAddInstanceOpen = false;
            this.ResetAddInstanceForm();
        }
        catch (Exception exception) when (exception is IOException or SocketException or InvalidOperationException)
        {
            this.AddInstanceError = exception.Message;
        }
        finally
        {
            this.IsAddingInstance = false;
        }
    }

    private void ResetAddInstanceForm()
    {
        this.NewInstanceName = string.Empty;
        this.NewInstanceRoot = string.Empty;
        this.NewInstanceGame = this.Games.FirstOrDefault();
        this.GameSearchText = string.Empty;
        this.NewInstanceLoader = string.Empty;
        this.NewInstanceSide = "Client";
        this.NewInstanceGameVersion = string.Empty;
        this.AddInstanceError = string.Empty;
    }

    private void RefreshGameSearch()
    {
        IEnumerable<GameInfo> matches = this.Games.Where(game =>
            game.Name.Contains(this.GameSearchText, StringComparison.OrdinalIgnoreCase)
            || game.Loaders.Any(loader => loader.Contains(this.GameSearchText, StringComparison.OrdinalIgnoreCase)));
        this.FilteredGames.Clear();
        foreach (GameInfo game in matches)
        {
            this.FilteredGames.Add(game);
        }

        this.OnPropertyChanged(nameof(this.IsGameSearchEmpty));
    }
}
