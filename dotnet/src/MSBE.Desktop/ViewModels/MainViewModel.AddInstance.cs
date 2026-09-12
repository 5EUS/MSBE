using System.Net.Sockets;

using CommunityToolkit.Mvvm.ComponentModel;
using CommunityToolkit.Mvvm.Input;

using MSBE.Client;

namespace MSBE.Desktop.ViewModels;

/// <content>Native instance registration workflow.</content>
internal sealed partial class MainViewModel
{
    /// <summary>Gets or sets whether the add-instance window is open.</summary>
    [ObservableProperty]
    public partial bool IsAddInstanceOpen { get; set; }

    /// <summary>Gets or sets the new instance name.</summary>
    [ObservableProperty]
    public partial string NewInstanceName { get; set; } = string.Empty;

    /// <summary>Gets or sets the game installation directory.</summary>
    [ObservableProperty]
    public partial string NewInstanceRoot { get; set; } = string.Empty;

    /// <summary>Gets or sets the deployment plan path.</summary>
    [ObservableProperty]
    public partial string NewInstancePlan { get; set; } = string.Empty;

    /// <summary>Gets or sets the loader declared by the plan.</summary>
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

    [RelayCommand]
    private void AddInstance()
    {
        this.AddInstanceError = string.Empty;
        this.IsAddInstanceOpen = true;
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
            string.IsNullOrWhiteSpace(this.NewInstancePlan) ||
            string.IsNullOrWhiteSpace(this.NewInstanceLoader))
        {
            this.AddInstanceError = "Name, game folder, plan file, and loader are required.";
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
                "--plan", this.NewInstancePlan.Trim(),
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
        this.NewInstancePlan = string.Empty;
        this.NewInstanceLoader = string.Empty;
        this.NewInstanceSide = "Client";
        this.NewInstanceGameVersion = string.Empty;
        this.AddInstanceError = string.Empty;
    }
}
