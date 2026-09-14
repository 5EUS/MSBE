using System.Diagnostics.CodeAnalysis;

using CommunityToolkit.Mvvm.ComponentModel;
using CommunityToolkit.Mvvm.Input;

using MSBE.Client;
using MSBE.Desktop.Resources;

namespace MSBE.Desktop.ViewModels;

/// <summary>A provider's link scheme and the application that opens it, as Settings → Link handlers shows it.</summary>
[SuppressMessage("Design", "CA1515:Consider making public types internal", Justification = "Compiled AXAML item templates reference this type directly.")]
public sealed partial class HandlerItem : ObservableObject
{
    /// <summary>Initializes a new instance of the <see cref="HandlerItem" /> class.</summary>
    /// <param name="status">The scheme's registration, as the daemon reported it.</param>
    internal HandlerItem(HandlerStatusInfo status)
    {
        this.Scheme = status.Scheme;
        this.Title = Strings.FormatHandlerTitle(status.Scheme);
        this.Update(status);
    }

    /// <summary>Gets the scheme.</summary>
    public string Scheme { get; }

    /// <summary>Gets the scheme as a heading.</summary>
    public string Title { get; }

    /// <summary>Gets or sets the provider whose links use the scheme.</summary>
    [ObservableProperty]
    public partial string ProviderText { get; set; } = string.Empty;

    /// <summary>Gets or sets which application opens the scheme's links.</summary>
    [ObservableProperty]
    public partial string OwnerText { get; set; } = string.Empty;

    /// <summary>Gets or sets whether this installation of MSBE opens the scheme's links.</summary>
    [ObservableProperty]
    [NotifyPropertyChangedFor(nameof(CanRegister))]
    public partial bool IsRegistered { get; set; }

    /// <summary>Gets or sets whether an MSBE registration exists, current or not.</summary>
    [ObservableProperty]
    public partial bool IsOwnedByMsbe { get; set; }

    /// <summary>Gets or sets whether MSBE asks before taking the scheme from another application.</summary>
    [ObservableProperty]
    [NotifyPropertyChangedFor(nameof(CanRegister))]
    public partial bool IsConfirmingReplace { get; set; }

    /// <summary>Gets or sets the question asked before taking the scheme from another application.</summary>
    [ObservableProperty]
    public partial string ReplaceText { get; set; } = string.Empty;

    /// <summary>Gets or sets why the last change was refused.</summary>
    [ObservableProperty]
    [NotifyPropertyChangedFor(nameof(HasError))]
    public partial string Error { get; set; } = string.Empty;

    /// <summary>Gets a value indicating whether MSBE can be registered for the scheme now.</summary>
    public bool CanRegister => !this.IsRegistered && !this.IsConfirmingReplace;

    /// <summary>Gets a value indicating whether the last change was refused.</summary>
    public bool HasError => this.Error.Length > 0;

    /// <summary>Gets the other application that opens the scheme's links, as the platform names it.</summary>
    internal string? OtherOwner { get; private set; }

    /// <summary>Shows the scheme's registration as the daemon now reports it.</summary>
    /// <param name="status">The scheme's registration.</param>
    internal void Update(HandlerStatusInfo status)
    {
        ArgumentNullException.ThrowIfNull(status);
        this.ProviderText = status.Provider is { } provider ? Strings.FormatHandlerProvider(provider) : string.Empty;
        this.OtherOwner = status.OwnerName;
        this.IsOwnedByMsbe = string.Equals(status.Owner, "msbe", StringComparison.Ordinal);
        this.IsRegistered = this.IsOwnedByMsbe && status.IsCurrent;
        this.OwnerText = status.Owner switch
        {
            "msbe" when status.IsCurrent && status.Previous is { } previous => Strings.FormatHandlerOwnedByMsbeReplacing(previous),
            "msbe" when status.IsCurrent => Strings.HandlerOwnedByMsbe,
            "msbe" => Strings.HandlerOwnedByMovedMsbe,
            "other" => Strings.FormatHandlerOwnedByOther(status.OwnerName ?? Strings.HandlerOtherApplication),
            _ => Strings.HandlerOwnedByNobody,
        };
        this.IsConfirmingReplace = false;
        this.Error = string.Empty;
    }

    [RelayCommand]
    private void CancelReplace() => this.IsConfirmingReplace = false;
}
