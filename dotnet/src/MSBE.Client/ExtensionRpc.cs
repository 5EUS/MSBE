using System.Text.Json;

namespace MSBE.Client;

/// <summary>Typed extension RPC methods.</summary>
/// <remarks>
/// Installed codecs and provider programs are admitted or refused one by one, so the list includes
/// refused extensions with their reasons. See <c>docs/18-wasm-extensions.md</c> §18.3.
/// </remarks>
public static class ExtensionRpc
{
    /// <summary>Lists the codecs and provider programs installed in the daemon's data directory.</summary>
    /// <param name="client">The daemon client.</param>
    /// <param name="cancellationToken">Cancels the pending request.</param>
    /// <returns>Every installed extension, admitted or refused, in the order the daemon found them.</returns>
    public static async Task<IReadOnlyList<ExtensionInfo>> ListExtensionsAsync(this IMsbeClient client, CancellationToken cancellationToken)
    {
        ArgumentNullException.ThrowIfNull(client);
        JsonElement result = await client.InvokeAsync("extension.list", writeParameters: null, cancellationToken).ConfigureAwait(false);
        var extensions = new List<ExtensionInfo>();
        foreach (JsonElement extension in result.EnumerateArray())
        {
            extensions.Add(new ExtensionInfo(
                Text(extension, "kind"),
                Text(extension, "path"),
                OptionalText(extension, "id"),
                OptionalText(extension, "version"),
                OptionalText(extension, "signer"),
                OptionalText(extension, "digest"),
                string.Equals(Text(extension, "status"), "active", StringComparison.Ordinal),
                OptionalText(extension, "reason")));
        }

        return extensions;
    }

    private static string Text(JsonElement element, string name) => element.GetProperty(name).GetString() ?? string.Empty;

    private static string? OptionalText(JsonElement element, string name) =>
        element.TryGetProperty(name, out JsonElement value) && value.ValueKind == JsonValueKind.String ? value.GetString() : null;
}
