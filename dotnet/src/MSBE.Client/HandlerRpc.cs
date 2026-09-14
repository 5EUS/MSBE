using System.Text.Json;

namespace MSBE.Client;

/// <summary>Typed link handler RPC methods.</summary>
/// <remarks>
/// Registration is per user and asks before taking a scheme from another application. See
/// <c>docs/07-browser-and-secrets.md</c> §7.4.
/// </remarks>
public static class HandlerRpc
{
    /// <summary>The error code of a registration refused because another application opens the scheme.</summary>
    public const int OwnedByAnotherApplication = -32040;

    /// <summary>Reports every scheme an enabled provider hands links over in.</summary>
    /// <param name="client">The daemon client.</param>
    /// <param name="cancellationToken">Cancels the pending request.</param>
    /// <returns>Each scheme's registration.</returns>
    public static async Task<IReadOnlyList<HandlerStatusInfo>> GetHandlerStatusAsync(this IMsbeClient client, CancellationToken cancellationToken)
    {
        ArgumentNullException.ThrowIfNull(client);
        JsonElement result = await client.InvokeAsync("handler.status", writeParameters: null, cancellationToken).ConfigureAwait(false);
        return [.. result.EnumerateArray().Select(Status)];
    }

    /// <summary>Registers MSBE to open a scheme's links.</summary>
    /// <param name="client">The daemon client.</param>
    /// <param name="scheme">The scheme.</param>
    /// <param name="replace">Whether to take the scheme over from the application that opens its links.</param>
    /// <param name="cancellationToken">Cancels the pending request.</param>
    /// <returns>The scheme's registration.</returns>
    /// <exception cref="MsbeRpcException">With <see cref="OwnedByAnotherApplication" />, when another application opens the scheme and <paramref name="replace" /> is false.</exception>
    public static async Task<HandlerStatusInfo> RegisterHandlerAsync(this IMsbeClient client, string scheme, bool replace, CancellationToken cancellationToken)
    {
        ArgumentNullException.ThrowIfNull(client);
        JsonElement result = await client.InvokeAsync(
            "handler.register",
            writer =>
            {
                writer.WriteStartObject();
                writer.WriteString("scheme", scheme);
                writer.WriteBoolean("replace", replace);
                writer.WriteEndObject();
            },
            cancellationToken).ConfigureAwait(false);
        return Status(result);
    }

    /// <summary>Removes MSBE's registration for a scheme, giving it back to the application MSBE replaced.</summary>
    /// <param name="client">The daemon client.</param>
    /// <param name="scheme">The scheme.</param>
    /// <param name="cancellationToken">Cancels the pending request.</param>
    /// <returns>The scheme's registration.</returns>
    public static async Task<HandlerStatusInfo> UnregisterHandlerAsync(this IMsbeClient client, string scheme, CancellationToken cancellationToken)
    {
        ArgumentNullException.ThrowIfNull(client);
        JsonElement result = await client.InvokeAsync(
            "handler.unregister",
            writer =>
            {
                writer.WriteStartObject();
                writer.WriteString("scheme", scheme);
                writer.WriteEndObject();
            },
            cancellationToken).ConfigureAwait(false);
        return Status(result);
    }

    private static HandlerStatusInfo Status(JsonElement handler)
    {
        JsonElement owner = handler.GetProperty("owner");
        return new HandlerStatusInfo(
            Text(handler, "scheme") ?? string.Empty,
            Text(handler, "provider"),
            Text(owner, "kind") ?? "nobody",
            Text(owner, "name"),
            handler.TryGetProperty("current", out JsonElement current) && current.ValueKind == JsonValueKind.True,
            Text(handler, "previous"));
    }

    private static string? Text(JsonElement element, string name) =>
        element.TryGetProperty(name, out JsonElement value) && value.ValueKind == JsonValueKind.String ? value.GetString() : null;
}
