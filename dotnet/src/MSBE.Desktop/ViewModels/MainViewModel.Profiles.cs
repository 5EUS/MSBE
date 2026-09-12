using System.Net.Sockets;

using CommunityToolkit.Mvvm.ComponentModel;
using CommunityToolkit.Mvvm.Input;

using MSBE.Client;

namespace MSBE.Desktop.ViewModels;

/// <content>Profile creation, cloning, and removal.</content>
internal sealed partial class MainViewModel
{
    /// <summary>Gets or sets whether the profile manager is open.</summary>
    [ObservableProperty]
    public partial bool IsProfileManagerOpen { get; set; }

    /// <summary>Gets or sets the name of a profile being created.</summary>
    [ObservableProperty]
    public partial string NewProfileName { get; set; } = string.Empty;

    /// <summary>Gets or sets the optional profile to clone.</summary>
    [ObservableProperty]
    public partial string? NewProfileSource { get; set; }

    /// <summary>Gets or sets whether a profile mutation is running.</summary>
    [ObservableProperty]
    [NotifyPropertyChangedFor(nameof(CanRemoveSelectedProfile))]
    public partial bool IsProfileMutationBusy { get; set; }

    /// <summary>Gets or sets the profile manager error.</summary>
    [ObservableProperty]
    [NotifyPropertyChangedFor(nameof(HasProfileMutationError))]
    public partial string ProfileMutationError { get; set; } = string.Empty;

    /// <summary>Gets a value indicating whether profile mutation failed.</summary>
    public bool HasProfileMutationError => !string.IsNullOrEmpty(this.ProfileMutationError);

    [RelayCommand]
    private void OpenProfileManager()
    {
        this.NewProfileName = string.Empty;
        this.NewProfileSource = this.SelectedProfile;
        this.ProfileMutationError = string.Empty;
        this.IsProfileManagerOpen = true;
    }

    [RelayCommand]
    private void CloseProfileManager() => this.IsProfileManagerOpen = false;

    [RelayCommand]
    private async Task CreateProfileAsync()
    {
        if (this.SelectedInstance is null || string.IsNullOrWhiteSpace(this.NewProfileName) || this.IsProfileMutationBusy)
        {
            return;
        }

        this.IsProfileMutationBusy = true;
        this.ProfileMutationError = string.Empty;
        try
        {
            string instance = this.SelectedInstance;
            string name = this.NewProfileName.Trim();
            List<string> arguments = ["--format", "json", "profile", "new", instance, name];
            if (!string.IsNullOrWhiteSpace(this.NewProfileSource))
            {
                arguments.Add("--from");
                arguments.Add(this.NewProfileSource);
            }

            CommandResult result = await this.client.RunCommandAsync(arguments, CancellationToken.None).ConfigureAwait(true);
            if (result.ExitCode != 0)
            {
                throw new InvalidOperationException(result.StandardError.Trim());
            }

            await this.LoadProfilesAsync(instance).ConfigureAwait(true);
            this.SelectedProfile = name;
            this.NewProfileName = string.Empty;
            this.StatusMessage = $"Created profile {name}.";
        }
        catch (Exception exception) when (exception is IOException or SocketException or InvalidOperationException)
        {
            this.ProfileMutationError = exception.Message;
        }
        finally
        {
            this.IsProfileMutationBusy = false;
        }
    }

    [RelayCommand]
    private async Task RemoveSelectedProfileAsync()
    {
        if (this.SelectedInstance is null || this.SelectedProfile is null || this.IsSelectedProfileDeployed || this.IsProfileMutationBusy)
        {
            return;
        }

        this.IsProfileMutationBusy = true;
        this.ProfileMutationError = string.Empty;
        try
        {
            string instance = this.SelectedInstance;
            string profile = this.SelectedProfile;
            CommandResult result = await this.client.RunCommandAsync(
                ["--format", "json", "profile", "remove", instance, profile],
                CancellationToken.None).ConfigureAwait(true);
            if (result.ExitCode != 0)
            {
                throw new InvalidOperationException(result.StandardError.Trim());
            }

            await this.LoadProfilesAsync(instance).ConfigureAwait(true);
            this.StatusMessage = $"Removed profile {profile}.";
        }
        catch (Exception exception) when (exception is IOException or SocketException or InvalidOperationException)
        {
            this.ProfileMutationError = exception.Message;
        }
        finally
        {
            this.IsProfileMutationBusy = false;
        }
    }
}
