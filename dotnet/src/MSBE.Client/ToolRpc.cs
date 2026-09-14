using System.Text.Json;

namespace MSBE.Client;

/// <summary>Typed external tool RPC methods.</summary>
/// <remarks>
/// The daemon runs a registered program for a queued item only while it still has the SHA-256 it was
/// registered with. See <c>docs/06-providers-and-policy.md</c> §6.5.
/// </remarks>
public static class ToolRpc
{
    /// <summary>Lists every enabled tool provider and the program registered for each.</summary>
    /// <param name="client">The daemon client.</param>
    /// <param name="cancellationToken">Cancels the pending request.</param>
    /// <returns>The tool providers, ordered by ID.</returns>
    public static async Task<IReadOnlyList<ToolStatusInfo>> ListToolsAsync(this IMsbeClient client, CancellationToken cancellationToken)
    {
        ArgumentNullException.ThrowIfNull(client);
        JsonElement result = await client.InvokeAsync("tool.list", writeParameters: null, cancellationToken).ConfigureAwait(false);
        return [.. result.EnumerateArray().Select(Status)];
    }

    /// <summary>Registers an installed program for a tool provider, pinning its SHA-256 and accepting the provider's terms.</summary>
    /// <param name="client">The daemon client.</param>
    /// <param name="provider">The provider ID.</param>
    /// <param name="program">The program's absolute path.</param>
    /// <param name="cancellationToken">Cancels the pending request.</param>
    /// <returns>The provider's tool state.</returns>
    public static async Task<ToolStatusInfo> RegisterToolAsync(this IMsbeClient client, string provider, string program, CancellationToken cancellationToken)
    {
        ArgumentNullException.ThrowIfNull(client);
        JsonElement result = await client.InvokeAsync(
            "tool.register",
            writer =>
            {
                writer.WriteStartObject();
                writer.WriteString("provider", provider);
                writer.WriteString("program", program);
                writer.WriteBoolean("accept_terms", value: true);
                writer.WriteEndObject();
            },
            cancellationToken).ConfigureAwait(false);
        return Status(result);
    }

    /// <summary>Forgets the program registered for a tool provider.</summary>
    /// <param name="client">The daemon client.</param>
    /// <param name="provider">The provider ID.</param>
    /// <param name="cancellationToken">Cancels the pending request.</param>
    /// <returns>The provider's tool state.</returns>
    public static async Task<ToolStatusInfo> ForgetToolAsync(this IMsbeClient client, string provider, CancellationToken cancellationToken)
    {
        ArgumentNullException.ThrowIfNull(client);
        JsonElement result = await client.InvokeAsync(
            "tool.forget",
            writer =>
            {
                writer.WriteStartObject();
                writer.WriteString("provider", provider);
                writer.WriteEndObject();
            },
            cancellationToken).ConfigureAwait(false);
        return Status(result);
    }

    private static ToolStatusInfo Status(JsonElement tool) => new(
        Text(tool, "provider") ?? string.Empty,
        Text(tool, "name") ?? string.Empty,
        Text(tool, "terms") ?? string.Empty,
        Text(tool, "program"),
        Text(tool, "sha256"),
        Text(tool, "state") ?? "unregistered");

    private static string? Text(JsonElement element, string name) =>
        element.TryGetProperty(name, out JsonElement value) && value.ValueKind == JsonValueKind.String ? value.GetString() : null;
}
