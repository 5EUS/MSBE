using System.Diagnostics.CodeAnalysis;

namespace MSBE.Desktop.ViewModels;

/// <summary>Top-level workspaces hosted by the desktop shell.</summary>
[SuppressMessage("Design", "CA1515:Consider making public types internal", Justification = "AXAML command parameters reference this enum directly.")]
public enum WorkspacePage
{
    /// <summary>The instance management workspace.</summary>
    Instances,

    /// <summary>The supported games workspace.</summary>
    Games,

    /// <summary>The provider-backed mod discovery workspace.</summary>
    Browse,

    /// <summary>Pack configuration, validation, and export.</summary>
    Pack,

    /// <summary>The queue of provider downloads.</summary>
    Downloads,

    /// <summary>The application settings workspace.</summary>
    Settings,
}
