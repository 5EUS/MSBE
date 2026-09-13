namespace MSBE.Client;

/// <summary>One file of a download queue item.</summary>
/// <param name="Provider">The provider.</param>
/// <param name="Project">The provider's project ID.</param>
/// <param name="Release">The provider's release ID.</param>
/// <param name="Name">The file name, empty until a link that arrived on its own is redeemed.</param>
/// <param name="State"><c>pending</c>, <c>awaiting_user</c>, <c>downloading</c> or <c>downloaded</c>.</param>
public sealed record DownloadFileInfo(string Provider, string Project, string Release, string Name, string State);
