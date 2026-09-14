namespace MSBE.Client;

/// <summary>An enabled provider, as the daemon describes it to clients.</summary>
/// <param name="Id">The provider ID.</param>
/// <param name="Name">Its display name.</param>
/// <param name="Prefix">The prefix its sources start with, such as <c>example:</c>, or <see langword="null" /> for HTTPS URL sources.</param>
/// <param name="IsSearchable">Whether it can be searched. One that cannot is added by reference or link.</param>
/// <param name="Acquisition">How it acquires files: <c>direct_https</c>, <c>user_action</c>, <c>browser_assisted</c> or <c>external_tool</c>.</param>
/// <param name="RequiresAuth">Whether it needs a key.</param>
/// <param name="IsSignedIn">Whether a key is kept for it, or set in the environment.</param>
/// <param name="RequiresAcknowledgement">Whether its terms must be accepted before it is used.</param>
/// <param name="IsAcknowledged">Whether its current terms are accepted, or it requires none.</param>
public sealed record ProviderInfo(
    string Id,
    string Name,
    string? Prefix,
    bool IsSearchable,
    string Acquisition,
    bool RequiresAuth,
    bool IsSignedIn,
    bool RequiresAcknowledgement,
    bool IsAcknowledged)
{
    /// <summary>Gets a value indicating whether its downloads wait for the user or for a program the user registers.</summary>
    public bool NeedsUser => !string.Equals(this.Acquisition, "direct_https", StringComparison.Ordinal);
}
