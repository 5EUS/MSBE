namespace MSBE.Client;

/// <summary>What the daemon's download queue accepted for a provider link. It never repeats the link, whose query carries a key.</summary>
/// <param name="Id">The queue item the link fills.</param>
/// <param name="Provider">The provider.</param>
/// <param name="Game">The plan game the link names.</param>
/// <param name="Project">The provider's project id.</param>
/// <param name="Release">The provider's release id.</param>
/// <param name="IsMatched">Whether the link fills a file a queued download was waiting on. One that does not becomes a download whose profile must be chosen.</param>
public sealed record HandoffReceiptInfo(long Id, string Provider, string Game, string Project, string Release, bool IsMatched);
