using System.Diagnostics.CodeAnalysis;
using System.Globalization;

using CommunityToolkit.Mvvm.ComponentModel;

using MSBE.Client;
using MSBE.Desktop.Resources;

namespace MSBE.Desktop.ViewModels;

/// <summary>A provider's sign-in and terms, as Settings → Accounts shows them. It holds a pasted key only until it is sent.</summary>
[SuppressMessage("Design", "CA1515:Consider making public types internal", Justification = "Compiled AXAML item templates reference this type directly.")]
public sealed partial class AccountItem : ObservableObject
{
    /// <summary>Initializes a new instance of the <see cref="AccountItem" /> class.</summary>
    /// <param name="status">The provider's state, as the daemon reported it.</param>
    internal AccountItem(AuthStatusInfo status)
    {
        this.Provider = status.Provider;
        this.Update(status);
    }

    /// <summary>Gets the provider ID.</summary>
    public string Provider { get; }

    /// <summary>Gets or sets the provider's display name.</summary>
    [ObservableProperty]
    public partial string Name { get; set; } = string.Empty;

    /// <summary>Gets or sets the key the user pasted. It is cleared as soon as it is sent.</summary>
    [ObservableProperty]
    [NotifyPropertyChangedFor(nameof(CanSignIn))]
    public partial string Key { get; set; } = string.Empty;

    /// <summary>Gets or sets whether a key is kept for the provider, or set in the environment.</summary>
    [ObservableProperty]
    [NotifyPropertyChangedFor(nameof(AcceptsKey))]
    [NotifyPropertyChangedFor(nameof(CanSignIn))]
    public partial bool IsSignedIn { get; set; }

    /// <summary>Gets or sets the page where the user finds or creates a key.</summary>
    [ObservableProperty]
    [NotifyPropertyChangedFor(nameof(HasKeyPage))]
    [NotifyPropertyChangedFor(nameof(AcceptsKey))]
    [NotifyPropertyChangedFor(nameof(CanSignIn))]
    public partial string? KeyPage { get; set; }

    /// <summary>Gets or sets the address of the provider's terms.</summary>
    [ObservableProperty]
    [NotifyPropertyChangedFor(nameof(HasTerms))]
    public partial string Terms { get; set; } = string.Empty;

    /// <summary>Gets or sets whether the provider's current terms must still be accepted.</summary>
    [ObservableProperty]
    [NotifyPropertyChangedFor(nameof(CanSignIn))]
    public partial bool NeedsAcknowledgement { get; set; }

    /// <summary>Gets or sets where sign-in stands.</summary>
    [ObservableProperty]
    public partial string SignInText { get; set; } = string.Empty;

    /// <summary>Gets or sets where the terms stand, or empty when the provider requires none.</summary>
    [ObservableProperty]
    public partial string TermsText { get; set; } = string.Empty;

    /// <summary>Gets or sets the requests the provider last reported remaining, one header per line.</summary>
    [ObservableProperty]
    [NotifyPropertyChangedFor(nameof(HasQuota))]
    public partial string QuotaText { get; set; } = string.Empty;

    /// <summary>Gets or sets why the last change was refused.</summary>
    [ObservableProperty]
    [NotifyPropertyChangedFor(nameof(HasError))]
    public partial string Error { get; set; } = string.Empty;

    /// <summary>Gets a value indicating whether the provider has a key page.</summary>
    public bool HasKeyPage => this.KeyPage is not null;

    /// <summary>Gets a value indicating whether the provider has terms to show.</summary>
    public bool HasTerms => this.Terms.Length > 0;

    /// <summary>Gets a value indicating whether a key can be pasted.</summary>
    public bool AcceptsKey => this.HasKeyPage && !this.IsSignedIn;

    /// <summary>Gets a value indicating whether the pasted key can be sent.</summary>
    public bool CanSignIn => this.AcceptsKey && !this.NeedsAcknowledgement && this.Key.Trim().Length > 0;

    /// <summary>Gets a value indicating whether a quota was reported.</summary>
    public bool HasQuota => this.QuotaText.Length > 0;

    /// <summary>Gets a value indicating whether the last change was refused.</summary>
    public bool HasError => this.Error.Length > 0;

    /// <summary>Shows the provider's state as the daemon now reports it.</summary>
    /// <param name="status">The provider's state.</param>
    internal void Update(AuthStatusInfo status)
    {
        ArgumentNullException.ThrowIfNull(status);
        this.Name = status.Name;
        this.IsSignedIn = status.IsSignedIn;
        this.KeyPage = status.KeyPage;
        this.Terms = status.Terms;
        this.NeedsAcknowledgement = status.RequiresAcknowledgement && !status.IsAcknowledged;
        this.SignInText = Describe(status);
        this.TermsText = (status.RequiresAcknowledgement, status.IsAcknowledged) switch
        {
            (false, _) => string.Empty,
            (true, true) => Strings.AccountTermsAccepted,
            (true, false) => Strings.AccountTermsNotAccepted,
        };
        this.QuotaText = string.Join(Environment.NewLine, status.Quota.Select(pair => Strings.FormatAccountQuota(pair.Value, pair.Key)));
        this.Error = string.Empty;
    }

    private static string Describe(AuthStatusInfo status)
    {
        if (!status.IsSignedIn)
        {
            return status.RequiresAuth ? Strings.AccountSignInRequired : Strings.AccountNotSignedIn;
        }

        if (string.Equals(status.Source, "environment", StringComparison.Ordinal))
        {
            return Strings.AccountSignedInFromEnvironment;
        }

        string source = status.Source switch
        {
            "keyring" => Strings.CredentialSourceKeyring,
            "encrypted file" => Strings.CredentialSourceEncryptedFile,
            _ => Strings.CredentialSourceMemory,
        };
        string signedIn = status.Account is { } account ? Strings.FormatAccountSignedInAs(account, source) : Strings.FormatAccountSignedIn(source);
        return status.LastUsed is { } used ? Strings.FormatAccountSignedInLastUsed(signedIn, used.ToLocalTime().ToString("g", CultureInfo.CurrentCulture)) : signedIn;
    }
}
