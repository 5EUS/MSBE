namespace MSBE.Client;

/// <summary>A provider's sign-in and terms. It never carries a key.</summary>
/// <param name="Provider">The provider ID.</param>
/// <param name="Name">Its display name.</param>
/// <param name="RequiresAuth">Whether it needs a key.</param>
/// <param name="IsSignedIn">Whether a key is kept for it, or set in the environment.</param>
/// <param name="Source">Where the key is: <c>environment</c>, <c>keyring</c>, <c>encrypted file</c> or <c>memory</c>.</param>
/// <param name="Account">The account the provider reported for the kept key.</param>
/// <param name="LastUsed">When the kept key was last read, to within a minute.</param>
/// <param name="KeyPage">The page where the user finds or creates a key.</param>
/// <param name="Terms">The address of its terms.</param>
/// <param name="RequiresAcknowledgement">Whether its terms must be accepted before it is used.</param>
/// <param name="IsAcknowledged">Whether its current terms are accepted, or it requires none.</param>
/// <param name="Quota">The requests its API last reported remaining, by response header.</param>
public sealed record AuthStatusInfo(
    string Provider,
    string Name,
    bool RequiresAuth,
    bool IsSignedIn,
    string? Source,
    string? Account,
    DateTimeOffset? LastUsed,
    string? KeyPage,
    string Terms,
    bool RequiresAcknowledgement,
    bool IsAcknowledged,
    IReadOnlyDictionary<string, long> Quota);
