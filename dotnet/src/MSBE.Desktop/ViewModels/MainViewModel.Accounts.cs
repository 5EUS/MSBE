using System.Collections.ObjectModel;
using System.Net.Sockets;
using System.Text.Json;

using CommunityToolkit.Mvvm.ComponentModel;
using CommunityToolkit.Mvvm.Input;

using MSBE.Client;
using MSBE.Desktop.Resources;

namespace MSBE.Desktop.ViewModels;

/// <content>
/// Settings → Accounts: sign-in and terms for the providers that need them. A pasted key goes to the
/// daemon, which checks it with the provider and keeps it; it never comes back.
/// </content>
internal sealed partial class MainViewModel
{
    /// <summary>Gets the providers that need a key or accepted terms, or accept a key.</summary>
    public ObservableCollection<AccountItem> Accounts { get; } = [];

    /// <summary>Gets or sets a summary of the accounts, or why they could not be read.</summary>
    [ObservableProperty]
    public partial string AccountsStatus { get; set; } = Strings.AccountsNotConnected;

    [RelayCommand]
    private async Task LoadAccountsAsync()
    {
        try
        {
            IReadOnlyList<AuthStatusInfo> statuses = await this.client.GetAuthStatusAsync(CancellationToken.None).ConfigureAwait(true);
            this.Accounts.Clear();
            foreach (AuthStatusInfo status in statuses)
            {
                this.Accounts.Add(new AccountItem(status));
            }

            this.AccountsStatus = statuses.Count == 0
                ? Strings.AccountsNone
                : Strings.FormatAccountsSummary(statuses.Count(status => status.IsSignedIn), statuses.Count);
        }
        catch (MsbeRpcException exception) when (exception.Code == MethodNotFoundCode)
        {
            this.AccountsStatus = Strings.AccountsUnsupported;
        }
        catch (Exception exception) when (exception is IOException or SocketException or JsonException or InvalidOperationException or KeyNotFoundException)
        {
            this.AccountsStatus = Strings.FormatAccountsFailed(exception.Message);
        }
    }

    [RelayCommand]
    private Task AcknowledgeTermsAsync(AccountItem? account) => account is null
        ? Task.CompletedTask
        : this.ChangeAccountAsync(
            account,
            () => this.client.AcknowledgeTermsAsync(account.Provider, CancellationToken.None),
            Strings.FormatAccountTermsAcceptedStatus(account.Name));

    [RelayCommand]
    private Task SignInAsync(AccountItem? account)
    {
        if (account is not { CanSignIn: true })
        {
            return Task.CompletedTask;
        }

        // The key leaves the view as it is sent, whatever the provider answers.
        string key = account.Key.Trim();
        account.Key = string.Empty;
        return this.ChangeAccountAsync(
            account,
            () => this.client.SignInAsync(account.Provider, key, CancellationToken.None),
            Strings.FormatAccountSignedInStatus(account.Name));
    }

    [RelayCommand]
    private Task SignOutAsync(AccountItem? account) => account is null
        ? Task.CompletedTask
        : this.ChangeAccountAsync(
            account,
            () => this.client.SignOutAsync(account.Provider, CancellationToken.None),
            Strings.FormatAccountSignedOutStatus(account.Name));

    /// <summary>Asks the daemon to change a provider's sign-in or terms, then shows the provider as it now is.</summary>
    private async Task ChangeAccountAsync(AccountItem account, Func<Task<AuthStatusInfo>> change, string done)
    {
        account.Error = string.Empty;
        try
        {
            account.Update(await change().ConfigureAwait(true));
            this.StatusMessage = done;
        }
        catch (Exception exception) when (exception is IOException or SocketException or JsonException or InvalidOperationException or KeyNotFoundException)
        {
            account.Error = exception.Message;
            return;
        }

        await this.LoadProvidersAsync().ConfigureAwait(true);
    }
}
