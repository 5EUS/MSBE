using System.Text.Json;

namespace MSBE.Client;

/// <summary>Typed provider description, sign-in and terms RPC methods.</summary>
/// <remarks>
/// A key reaches the daemon only through <see cref="SignInAsync" />. The daemon checks it with the
/// provider, keeps it in the credential store, and never answers with it. See
/// <c>docs/07-browser-and-secrets.md</c> §7.5.
/// </remarks>
public static class AccountRpc
{
    /// <summary>Describes every enabled provider.</summary>
    /// <param name="client">The daemon client.</param>
    /// <param name="cancellationToken">Cancels the pending request.</param>
    /// <returns>The providers, ordered by ID.</returns>
    public static async Task<IReadOnlyList<ProviderInfo>> ListProvidersAsync(this IMsbeClient client, CancellationToken cancellationToken)
    {
        ArgumentNullException.ThrowIfNull(client);
        JsonElement result = await client.InvokeAsync("provider.list", writeParameters: null, cancellationToken).ConfigureAwait(false);
        var providers = new List<ProviderInfo>();
        foreach (JsonElement provider in result.EnumerateArray())
        {
            providers.Add(new ProviderInfo(
                Text(provider, "id"),
                Text(provider, "name"),
                OptionalText(provider, "prefix"),
                Flag(provider, "search"),
                Text(provider, "acquisition"),
                Flag(provider, "requires_auth"),
                Flag(provider, "signed_in"),
                Flag(provider, "ack_required"),
                Flag(provider, "acknowledged")));
        }

        return providers;
    }

    /// <summary>Reads the sign-in and terms of every provider that needs a key or accepted terms, or accepts a key.</summary>
    /// <param name="client">The daemon client.</param>
    /// <param name="cancellationToken">Cancels the pending request.</param>
    /// <returns>Each such provider's state, ordered by ID.</returns>
    public static async Task<IReadOnlyList<AuthStatusInfo>> GetAuthStatusAsync(this IMsbeClient client, CancellationToken cancellationToken)
    {
        ArgumentNullException.ThrowIfNull(client);
        JsonElement result = await client.InvokeAsync("auth.status", writeParameters: null, cancellationToken).ConfigureAwait(false);
        return [.. result.EnumerateArray().Select(Status)];
    }

    /// <summary>Accepts a provider's current terms.</summary>
    /// <param name="client">The daemon client.</param>
    /// <param name="provider">The provider ID.</param>
    /// <param name="cancellationToken">Cancels the pending request.</param>
    /// <returns>The provider's state.</returns>
    public static Task<AuthStatusInfo> AcknowledgeTermsAsync(this IMsbeClient client, string provider, CancellationToken cancellationToken) =>
        ChangeAsync(client, "auth.acknowledge", provider, key: null, cancellationToken);

    /// <summary>Checks a key with its provider and keeps it. The provider's terms must be accepted first.</summary>
    /// <param name="client">The daemon client.</param>
    /// <param name="provider">The provider ID.</param>
    /// <param name="key">The key the user pasted.</param>
    /// <param name="cancellationToken">Cancels the pending request.</param>
    /// <returns>The provider's state.</returns>
    /// <exception cref="MsbeRpcException">The provider refused the key, or its terms are not accepted.</exception>
    public static Task<AuthStatusInfo> SignInAsync(this IMsbeClient client, string provider, string key, CancellationToken cancellationToken)
    {
        ArgumentException.ThrowIfNullOrEmpty(key);
        return ChangeAsync(client, "auth.login", provider, key, cancellationToken);
    }

    /// <summary>Forgets the key kept for a provider. A key set in the environment is unaffected.</summary>
    /// <param name="client">The daemon client.</param>
    /// <param name="provider">The provider ID.</param>
    /// <param name="cancellationToken">Cancels the pending request.</param>
    /// <returns>The provider's state.</returns>
    public static Task<AuthStatusInfo> SignOutAsync(this IMsbeClient client, string provider, CancellationToken cancellationToken) =>
        ChangeAsync(client, "auth.logout", provider, key: null, cancellationToken);

    private static async Task<AuthStatusInfo> ChangeAsync(IMsbeClient client, string method, string provider, string? key, CancellationToken cancellationToken)
    {
        ArgumentNullException.ThrowIfNull(client);
        JsonElement result = await client.InvokeAsync(
            method,
            writer =>
            {
                writer.WriteStartObject();
                writer.WriteString("provider", provider);
                if (key is not null)
                {
                    writer.WriteString("token", key);
                }

                writer.WriteEndObject();
            },
            cancellationToken).ConfigureAwait(false);
        return Status(result);
    }

    private static AuthStatusInfo Status(JsonElement status)
    {
        var quota = new Dictionary<string, long>(StringComparer.Ordinal);
        if (status.TryGetProperty("quota", out JsonElement remaining) && remaining.ValueKind == JsonValueKind.Object)
        {
            foreach (JsonProperty header in remaining.EnumerateObject())
            {
                quota[header.Name] = header.Value.TryGetInt64(out long count) ? count : long.MaxValue;
            }
        }

        DateTimeOffset? lastUsed = status.TryGetProperty("last_used", out JsonElement used) && used.ValueKind == JsonValueKind.Number
            ? DateTimeOffset.FromUnixTimeSeconds(used.GetInt64())
            : null;
        return new AuthStatusInfo(
            Text(status, "provider"),
            Text(status, "name"),
            Flag(status, "requires_auth"),
            Flag(status, "signed_in"),
            OptionalText(status, "source"),
            OptionalText(status, "account"),
            lastUsed,
            OptionalText(status, "key_page"),
            Text(status, "terms"),
            Flag(status, "ack_required"),
            Flag(status, "acknowledged"),
            quota);
    }

    private static string Text(JsonElement element, string name) => OptionalText(element, name) ?? string.Empty;

    private static string? OptionalText(JsonElement element, string name) =>
        element.TryGetProperty(name, out JsonElement value) && value.ValueKind == JsonValueKind.String ? value.GetString() : null;

    private static bool Flag(JsonElement element, string name) =>
        element.TryGetProperty(name, out JsonElement value) && value.ValueKind == JsonValueKind.True;
}
