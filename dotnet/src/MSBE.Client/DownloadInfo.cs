namespace MSBE.Client;

/// <summary>One item in the daemon's download queue: a requested source and the files it resolved to.</summary>
/// <param name="Id">The daemon's identifier for the item.</param>
/// <param name="Title">The display title the client that queued it supplied.</param>
/// <param name="Source">The source it was queued for, or <see langword="null" /> for an item a link created on its own.</param>
/// <param name="Instance">The instance it is added to, or <see langword="null" /> until one is chosen.</param>
/// <param name="Profile">The profile it is added to, or <see langword="null" /> until one is chosen.</param>
/// <param name="State">The state's kind, such as <c>queued</c>, <c>awaiting_user</c> or <c>completed</c>.</param>
/// <param name="Page">The page the user starts the next download on, while the item awaits the user.</param>
/// <param name="Message">Why the item failed, when it did.</param>
/// <param name="Files">The files it resolved to.</param>
/// <param name="Added">The mods it added.</param>
/// <param name="Skipped">Mods that were already in the profile.</param>
/// <param name="Warnings">Requirements resolution could not meet, and declared incompatibilities.</param>
public sealed record DownloadInfo(
    long Id,
    string? Title,
    string? Source,
    string? Instance,
    string? Profile,
    string State,
    string? Page,
    string? Message,
    IReadOnlyList<DownloadFileInfo> Files,
    IReadOnlyList<string> Added,
    IReadOnlyList<string> Skipped,
    IReadOnlyList<string> Warnings);
