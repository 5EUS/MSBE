using System.Diagnostics.CodeAnalysis;

namespace MSBE.Desktop.ViewModels;

/// <summary>Where a download is in the queue's lifecycle.</summary>
[SuppressMessage("Design", "CA1515:Consider making public types internal", Justification = "Exposed by DownloadQueueItem, which compiled AXAML item templates reference.")]
public enum DownloadState
{
    /// <summary>Waiting for earlier downloads to finish.</summary>
    Queued,

    /// <summary>Being downloaded and added to its profile.</summary>
    Downloading,

    /// <summary>Added to its profile.</summary>
    Completed,

    /// <summary>Stopped by an error.</summary>
    Failed,
}
