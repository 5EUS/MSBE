using System.Diagnostics.CodeAnalysis;

namespace MSBE.Desktop.ViewModels;

/// <summary>Top-level workspaces hosted by the desktop shell.</summary>
[SuppressMessage("Design", "CA1515:Consider making public types internal", Justification = "AXAML command parameters reference this enum directly.")]
public enum WorkspacePage
{
    /// <summary>The instance management workspace.</summary>
    Instances,

    /// <summary>The game plan workspace.</summary>
    Games,

    /// <summary>The provider management workspace.</summary>
    Providers,

    /// <summary>The application settings workspace.</summary>
    Settings,
}
