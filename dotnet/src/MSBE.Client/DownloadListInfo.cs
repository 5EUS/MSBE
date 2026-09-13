namespace MSBE.Client;

/// <summary>The daemon's download queue, or the part of it that changed.</summary>
/// <param name="Next">The revision to request changes after next time.</param>
/// <param name="IsPaused">Whether the queue is paused. Links are still received.</param>
/// <param name="Order">Every item's ID in queue order; an item missing from it was cleared.</param>
/// <param name="Items">The items changed after the requested revision.</param>
public sealed record DownloadListInfo(long Next, bool IsPaused, IReadOnlyList<long> Order, IReadOnlyList<DownloadInfo> Items);
