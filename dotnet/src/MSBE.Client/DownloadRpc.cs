using System.Text.Json;

namespace MSBE.Client;

/// <summary>Typed download queue RPC methods.</summary>
/// <remarks>
/// The daemon owns the queue, so downloads continue while no client is open, and a link clicked in a
/// browser lands in the same queue. See <c>docs/03-architecture.md</c> §3.4.
/// </remarks>
public static class DownloadRpc
{
    /// <summary>Queues a source to download and add to a profile.</summary>
    /// <param name="client">The daemon client.</param>
    /// <param name="instance">The instance.</param>
    /// <param name="profile">The profile.</param>
    /// <param name="source">A provider reference or HTTPS URL.</param>
    /// <param name="withDependencies">Whether required dependencies are downloaded and added with it.</param>
    /// <param name="title">A display title to keep with the item.</param>
    /// <param name="cancellationToken">Cancels the pending request.</param>
    /// <returns>The queued item, or the unfinished item already queued for the same source and profile.</returns>
    public static async Task<DownloadInfo> EnqueueDownloadAsync(this IMsbeClient client, string instance, string profile, string source, bool withDependencies, string? title, CancellationToken cancellationToken)
    {
        ArgumentNullException.ThrowIfNull(client);
        JsonElement result = await client.InvokeAsync(
            "download.enqueue",
            writer =>
            {
                writer.WriteStartObject();
                writer.WriteString("instance", instance);
                writer.WriteString("profile", profile);
                writer.WriteString("source", source);
                writer.WriteBoolean("with_deps", withDependencies);
                if (title is not null)
                {
                    writer.WriteString("title", title);
                }

                writer.WriteEndObject();
            },
            cancellationToken).ConfigureAwait(false);
        return Item(result);
    }

    /// <summary>Reads the items that changed after a queue revision.</summary>
    /// <param name="client">The daemon client.</param>
    /// <param name="after">The revision last seen, or zero for the whole queue.</param>
    /// <param name="cancellationToken">Cancels the pending request.</param>
    /// <returns>The changed items, with every item's ID in queue order.</returns>
    public static async Task<DownloadListInfo> ListDownloadsAsync(this IMsbeClient client, long after, CancellationToken cancellationToken)
    {
        ArgumentNullException.ThrowIfNull(client);
        JsonElement result = await client.InvokeAsync(
            "download.list",
            writer =>
            {
                writer.WriteStartObject();
                writer.WriteNumber("after", after);
                writer.WriteEndObject();
            },
            cancellationToken).ConfigureAwait(false);
        return List(result);
    }

    /// <summary>Holds one item before its next step, or the whole queue.</summary>
    /// <param name="client">The daemon client.</param>
    /// <param name="id">The item, or <see langword="null" /> for the whole queue.</param>
    /// <param name="cancellationToken">Cancels the pending request.</param>
    /// <returns>The whole queue.</returns>
    public static Task<DownloadListInfo> PauseDownloadsAsync(this IMsbeClient client, long? id, CancellationToken cancellationToken) =>
        ListCallAsync(client, "download.pause", id, cancellationToken);

    /// <summary>Releases one paused item, or the whole queue.</summary>
    /// <param name="client">The daemon client.</param>
    /// <param name="id">The item, or <see langword="null" /> for the whole queue.</param>
    /// <param name="cancellationToken">Cancels the pending request.</param>
    /// <returns>The whole queue.</returns>
    public static Task<DownloadListInfo> ResumeDownloadsAsync(this IMsbeClient client, long? id, CancellationToken cancellationToken) =>
        ListCallAsync(client, "download.resume", id, cancellationToken);

    /// <summary>Cancels an item that is not being added.</summary>
    /// <param name="client">The daemon client.</param>
    /// <param name="id">The item.</param>
    /// <param name="cancellationToken">Cancels the pending request.</param>
    /// <returns>The cancelled item.</returns>
    public static Task<DownloadInfo> CancelDownloadAsync(this IMsbeClient client, long id, CancellationToken cancellationToken) =>
        ItemCallAsync(client, "download.cancel", id, cancellationToken);

    /// <summary>Queues a failed or cancelled item again, keeping the files it already downloaded.</summary>
    /// <param name="client">The daemon client.</param>
    /// <param name="id">The item.</param>
    /// <param name="cancellationToken">Cancels the pending request.</param>
    /// <returns>The item, queued again or waiting to be added again.</returns>
    public static Task<DownloadInfo> RetryDownloadAsync(this IMsbeClient client, long id, CancellationToken cancellationToken) =>
        ItemCallAsync(client, "download.retry", id, cancellationToken);

    /// <summary>Moves an item to a position in the queue order.</summary>
    /// <param name="client">The daemon client.</param>
    /// <param name="id">The item.</param>
    /// <param name="position">Its new zero-based position among every item.</param>
    /// <param name="cancellationToken">Cancels the pending request.</param>
    /// <returns>The whole queue.</returns>
    public static async Task<DownloadListInfo> MoveDownloadAsync(this IMsbeClient client, long id, int position, CancellationToken cancellationToken)
    {
        ArgumentNullException.ThrowIfNull(client);
        JsonElement result = await client.InvokeAsync(
            "download.move",
            writer =>
            {
                writer.WriteStartObject();
                writer.WriteNumber("id", id);
                writer.WriteNumber("position", position);
                writer.WriteEndObject();
            },
            cancellationToken).ConfigureAwait(false);
        return List(result);
    }

    /// <summary>Chooses the profile for an item a link created on its own.</summary>
    /// <param name="client">The daemon client.</param>
    /// <param name="id">The item.</param>
    /// <param name="instance">The instance to add it to.</param>
    /// <param name="profile">The profile to add it to.</param>
    /// <param name="cancellationToken">Cancels the pending request.</param>
    /// <returns>The item with its profile.</returns>
    public static async Task<DownloadInfo> ConfirmDownloadAsync(this IMsbeClient client, long id, string instance, string profile, CancellationToken cancellationToken)
    {
        ArgumentNullException.ThrowIfNull(client);
        JsonElement result = await client.InvokeAsync(
            "download.confirm",
            writer =>
            {
                writer.WriteStartObject();
                writer.WriteNumber("id", id);
                writer.WriteString("instance", instance);
                writer.WriteString("profile", profile);
                writer.WriteEndObject();
            },
            cancellationToken).ConfigureAwait(false);
        return Item(result);
    }

    /// <summary>Removes completed, failed and cancelled items.</summary>
    /// <param name="client">The daemon client.</param>
    /// <param name="cancellationToken">Cancels the pending request.</param>
    /// <returns>The whole queue.</returns>
    public static async Task<DownloadListInfo> ClearDownloadsAsync(this IMsbeClient client, CancellationToken cancellationToken)
    {
        ArgumentNullException.ThrowIfNull(client);
        JsonElement result = await client.InvokeAsync("download.clear", writeParameters: null, cancellationToken).ConfigureAwait(false);
        return List(result);
    }

    /// <summary>Hands a provider link to the download queue, which redeems and downloads it at once because its key expires.</summary>
    /// <param name="client">The daemon client.</param>
    /// <param name="link">The link a provider page handed over.</param>
    /// <param name="cancellationToken">Cancels the pending request.</param>
    /// <returns>What the queue accepted, without the link.</returns>
    public static async Task<HandoffReceiptInfo> SubmitHandoffAsync(this IMsbeClient client, string link, CancellationToken cancellationToken)
    {
        ArgumentNullException.ThrowIfNull(client);
        JsonElement result = await client.InvokeAsync(
            "handoff.submit",
            writer =>
            {
                writer.WriteStartObject();
                writer.WriteString("uri", link);
                writer.WriteEndObject();
            },
            cancellationToken).ConfigureAwait(false);
        return new HandoffReceiptInfo(
            result.GetProperty("id").GetInt64(),
            Text(result, "provider"),
            Text(result, "game"),
            Text(result, "project"),
            Text(result, "release"),
            result.GetProperty("matched").GetBoolean());
    }

    private static async Task<DownloadListInfo> ListCallAsync(IMsbeClient client, string method, long? id, CancellationToken cancellationToken)
    {
        ArgumentNullException.ThrowIfNull(client);
        Action<Utf8JsonWriter>? parameters = null;
        if (id is long item)
        {
            parameters = writer =>
            {
                writer.WriteStartObject();
                writer.WriteNumber("id", item);
                writer.WriteEndObject();
            };
        }

        JsonElement result = await client.InvokeAsync(method, parameters, cancellationToken).ConfigureAwait(false);
        return List(result);
    }

    private static async Task<DownloadInfo> ItemCallAsync(IMsbeClient client, string method, long id, CancellationToken cancellationToken)
    {
        ArgumentNullException.ThrowIfNull(client);
        JsonElement result = await client.InvokeAsync(
            method,
            writer =>
            {
                writer.WriteStartObject();
                writer.WriteNumber("id", id);
                writer.WriteEndObject();
            },
            cancellationToken).ConfigureAwait(false);
        return Item(result);
    }

    private static DownloadListInfo List(JsonElement result)
    {
        var order = new List<long>();
        foreach (JsonElement id in result.GetProperty("order").EnumerateArray())
        {
            order.Add(id.GetInt64());
        }

        var items = new List<DownloadInfo>();
        foreach (JsonElement item in result.GetProperty("items").EnumerateArray())
        {
            items.Add(Item(item));
        }

        return new DownloadListInfo(result.GetProperty("next").GetInt64(), result.GetProperty("paused").GetBoolean(), order, items);
    }

    private static DownloadInfo Item(JsonElement item)
    {
        JsonElement state = item.GetProperty("state");
        string? instance = null;
        string? profile = null;
        if (item.TryGetProperty("target", out JsonElement target) && target.ValueKind == JsonValueKind.Object)
        {
            instance = OptionalText(target, "instance");
            profile = OptionalText(target, "profile");
        }

        var files = new List<DownloadFileInfo>();
        if (item.TryGetProperty("files", out JsonElement listed) && listed.ValueKind == JsonValueKind.Array)
        {
            foreach (JsonElement file in listed.EnumerateArray())
            {
                string fileState = file.TryGetProperty("state", out JsonElement kind) ? Text(kind, "kind") : string.Empty;
                files.Add(new DownloadFileInfo(Text(file, "provider"), Text(file, "project"), Text(file, "release"), Text(file, "name"), fileState));
            }
        }

        return new DownloadInfo(
            item.GetProperty("id").GetInt64(),
            OptionalText(item, "title"),
            OptionalText(item, "source"),
            instance,
            profile,
            Text(state, "kind"),
            OptionalText(state, "page"),
            OptionalText(state, "message"),
            files,
            Texts(item, "added"),
            Texts(item, "skipped"),
            Texts(item, "warnings"));
    }

    private static string Text(JsonElement element, string property) => OptionalText(element, property) ?? string.Empty;

    private static string? OptionalText(JsonElement element, string property) =>
        element.TryGetProperty(property, out JsonElement value) && value.ValueKind == JsonValueKind.String ? value.GetString() : null;

    private static List<string> Texts(JsonElement element, string property)
    {
        var texts = new List<string>();
        if (element.TryGetProperty(property, out JsonElement values) && values.ValueKind == JsonValueKind.Array)
        {
            foreach (JsonElement value in values.EnumerateArray())
            {
                if (value.GetString() is { } text)
                {
                    texts.Add(text);
                }
            }
        }

        return texts;
    }
}
