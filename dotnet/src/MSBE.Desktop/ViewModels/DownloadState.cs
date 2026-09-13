using System.Diagnostics.CodeAnalysis;

namespace MSBE.Desktop.ViewModels;

/// <summary>Where a download is in the daemon queue's lifecycle.</summary>
[SuppressMessage("Design", "CA1515:Consider making public types internal", Justification = "Exposed by DownloadQueueItem, which compiled AXAML item templates reference.")]
public enum DownloadState
{
    /// <summary>Waiting for earlier downloads.</summary>
    Queued,

    /// <summary>Held until it is resumed.</summary>
    Paused,

    /// <summary>Its source is being resolved to the files it needs.</summary>
    Resolving,

    /// <summary>Its files are downloading.</summary>
    Downloading,

    /// <summary>Waiting for the user to start a download on a provider page.</summary>
    AwaitingUser,

    /// <summary>Downloaded, and waiting to be added or for its profile to be chosen.</summary>
    Downloaded,

    /// <summary>Being added to its profile.</summary>
    Adding,

    /// <summary>Added to its profile.</summary>
    Completed,

    /// <summary>Stopped by an error.</summary>
    Failed,

    /// <summary>Cancelled by the user.</summary>
    Cancelled,
}
