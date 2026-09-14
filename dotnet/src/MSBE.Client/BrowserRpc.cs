using System.Text.Json;

namespace MSBE.Client;

/// <summary>Typed MSBE browser RPC methods.</summary>
/// <remarks>
/// The daemon starts and drives the browser, which runs in its own process with no binding into the
/// client. See <c>docs/07-browser-and-secrets.md</c> §7.2.
/// </remarks>
public static class BrowserRpc
{
    /// <summary>Reads the browser's state and how many downloads wait on a page.</summary>
    /// <param name="client">The daemon client.</param>
    /// <param name="cancellationToken">Cancels the pending request.</param>
    /// <returns>The browser's state.</returns>
    public static async Task<BrowserStatusInfo> GetBrowserStatusAsync(this IMsbeClient client, CancellationToken cancellationToken)
    {
        ArgumentNullException.ThrowIfNull(client);
        JsonElement result = await client.InvokeAsync("browser.status", writeParameters: null, cancellationToken).ConfigureAwait(false);
        return Status(result);
    }

    /// <summary>Sends the browser to the page a download waits on, starting it when needed.</summary>
    /// <param name="client">The daemon client.</param>
    /// <param name="id">The download, or <see langword="null" /> for the next one waiting on a page.</param>
    /// <param name="autoAdvance">Whether to go to the next page once a download or link arrives, or <see langword="null" /> to leave it unchanged.</param>
    /// <param name="cancellationToken">Cancels the pending request.</param>
    /// <returns>The browser's state.</returns>
    public static async Task<BrowserStatusInfo> OpenBrowserAsync(this IMsbeClient client, long? id, bool? autoAdvance, CancellationToken cancellationToken)
    {
        ArgumentNullException.ThrowIfNull(client);
        JsonElement result = await client.InvokeAsync(
            "browser.open",
            writer =>
            {
                writer.WriteStartObject();
                if (id is long item)
                {
                    writer.WriteNumber("id", item);
                }

                if (autoAdvance is bool advance)
                {
                    writer.WriteBoolean("auto_advance", advance);
                }

                writer.WriteEndObject();
            },
            cancellationToken).ConfigureAwait(false);
        return Status(result);
    }

    /// <summary>Closes the browser.</summary>
    /// <param name="client">The daemon client.</param>
    /// <param name="cancellationToken">Cancels the pending request.</param>
    /// <returns>The browser's state.</returns>
    public static async Task<BrowserStatusInfo> CloseBrowserAsync(this IMsbeClient client, CancellationToken cancellationToken)
    {
        ArgumentNullException.ThrowIfNull(client);
        JsonElement result = await client.InvokeAsync("browser.close", writeParameters: null, cancellationToken).ConfigureAwait(false);
        return Status(result);
    }

    private static BrowserStatusInfo Status(JsonElement result) => new(
        result.GetProperty("running").GetBoolean(),
        Text(result, "provider"),
        Number(result, "item"),
        Text(result, "page"),
        Number(result, "position"),
        Number(result, "waiting") ?? 0,
        Text(result, "url"),
        Text(result, "title"),
        result.TryGetProperty("auto_advance", out JsonElement advance) && advance.ValueKind == JsonValueKind.True,
        Text(result, "message"));

    private static string? Text(JsonElement element, string property) =>
        element.TryGetProperty(property, out JsonElement value) && value.ValueKind == JsonValueKind.String ? value.GetString() : null;

    private static long? Number(JsonElement element, string property) =>
        element.TryGetProperty(property, out JsonElement value) && value.ValueKind == JsonValueKind.Number ? value.GetInt64() : null;
}
