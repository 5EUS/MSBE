namespace MSBE.Client;

/// <summary>The MSBE browser, and the downloads waiting on a page.</summary>
/// <param name="IsRunning">Whether the browser window is open.</param>
/// <param name="Provider">The provider whose pages it shows, while it is open.</param>
/// <param name="Item">The download whose page it was sent to.</param>
/// <param name="Page">The page it was sent to.</param>
/// <param name="Position">Where that page is among the files waiting on a page, counting from one.</param>
/// <param name="Waiting">How many files wait on a page.</param>
/// <param name="Location">The address the browser shows, which the user may have followed away from the page.</param>
/// <param name="Title">The title of the page the browser shows.</param>
/// <param name="IsAutoAdvancing">Whether the browser goes to the next page once a download or link arrives.</param>
/// <param name="Message">Why the last capture was refused, or why the browser stopped.</param>
public sealed record BrowserStatusInfo(
    bool IsRunning,
    string? Provider,
    long? Item,
    string? Page,
    long? Position,
    long Waiting,
    string? Location,
    string? Title,
    bool IsAutoAdvancing,
    string? Message);
