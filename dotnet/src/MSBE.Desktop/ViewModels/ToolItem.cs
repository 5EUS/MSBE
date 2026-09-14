using System.Diagnostics.CodeAnalysis;

using CommunityToolkit.Mvvm.ComponentModel;

using MSBE.Client;
using MSBE.Desktop.Resources;

namespace MSBE.Desktop.ViewModels;

/// <summary>A tool provider and the program registered for it, as Settings → External tools shows it.</summary>
[SuppressMessage("Design", "CA1515:Consider making public types internal", Justification = "Compiled AXAML item templates reference this type directly.")]
public sealed partial class ToolItem : ObservableObject
{
    /// <summary>Initializes a new instance of the <see cref="ToolItem" /> class.</summary>
    /// <param name="status">The provider's tool state, as the daemon reported it.</param>
    internal ToolItem(ToolStatusInfo status)
    {
        this.Provider = status.Provider;
        this.Update(status);
    }

    /// <summary>Gets the provider ID.</summary>
    public string Provider { get; }

    /// <summary>Gets or sets the provider's display name.</summary>
    [ObservableProperty]
    public partial string Name { get; set; } = string.Empty;

    /// <summary>Gets or sets the address of the provider's terms, which registering a program accepts.</summary>
    [ObservableProperty]
    public partial string Terms { get; set; } = string.Empty;

    /// <summary>Gets or sets where the registration stands.</summary>
    [ObservableProperty]
    public partial string StateText { get; set; } = string.Empty;

    /// <summary>Gets or sets the registered program and the start of its SHA-256.</summary>
    [ObservableProperty]
    [NotifyPropertyChangedFor(nameof(HasProgram))]
    public partial string ProgramText { get; set; } = string.Empty;

    /// <summary>Gets or sets whether the registered program can run.</summary>
    [ObservableProperty]
    public partial bool IsRegistered { get; set; }

    /// <summary>Gets or sets the absolute path of the program to register.</summary>
    [ObservableProperty]
    [NotifyPropertyChangedFor(nameof(CanRegister))]
    public partial string ProgramPath { get; set; } = string.Empty;

    /// <summary>Gets or sets whether the user accepts the provider's terms.</summary>
    [ObservableProperty]
    [NotifyPropertyChangedFor(nameof(CanRegister))]
    public partial bool AcceptsTerms { get; set; }

    /// <summary>Gets or sets why the last change was refused.</summary>
    [ObservableProperty]
    [NotifyPropertyChangedFor(nameof(HasError))]
    public partial string Error { get; set; } = string.Empty;

    /// <summary>Gets a value indicating whether a program is registered, whether or not it can run.</summary>
    public bool HasProgram => this.ProgramText.Length > 0;

    /// <summary>Gets a value indicating whether the program path is absolute and the terms are accepted.</summary>
    public bool CanRegister => this.AcceptsTerms && this.ProgramPath.Trim() is { Length: > 0 } path && Path.IsPathFullyQualified(path);

    /// <summary>Gets a value indicating whether the last change was refused.</summary>
    public bool HasError => this.Error.Length > 0;

    /// <summary>Shows the provider's tool state as the daemon now reports it.</summary>
    /// <param name="status">The provider's tool state.</param>
    internal void Update(ToolStatusInfo status)
    {
        ArgumentNullException.ThrowIfNull(status);
        this.Name = status.Name;
        this.Terms = status.Terms;
        this.IsRegistered = string.Equals(status.State, "registered", StringComparison.Ordinal);
        string? shownDigest = status.Sha256 is { Length: > 12 } sha256 ? sha256[..12] : status.Sha256;
        this.ProgramText = status.Program is { } program ? Strings.FormatToolProgram(program, shownDigest) : string.Empty;
        this.StateText = status.State switch
        {
            "registered" => Strings.ToolRegistered,
            "changed" => Strings.ToolChanged,
            "missing" => Strings.ToolMissing,
            _ => Strings.ToolUnregistered,
        };
        this.ProgramPath = status.Program ?? string.Empty;
        this.AcceptsTerms = false;
        this.Error = string.Empty;
    }
}
