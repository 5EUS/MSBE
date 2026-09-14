namespace MSBE.Client;

/// <summary>A provider that fetches content with a program the user installed, and the program registered for it.</summary>
/// <param name="Provider">The provider ID.</param>
/// <param name="Name">Its display name.</param>
/// <param name="Terms">The address of its terms, which registering a program accepts.</param>
/// <param name="Program">The registered program, with every link resolved.</param>
/// <param name="Sha256">The program's SHA-256 when it was registered, as lowercase hex.</param>
/// <param name="State"><c>registered</c>, <c>unregistered</c>, <c>changed</c> when the program no longer has that SHA-256, or <c>missing</c>.</param>
public sealed record ToolStatusInfo(string Provider, string Name, string Terms, string? Program, string? Sha256, string State);
