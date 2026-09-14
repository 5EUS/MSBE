using System.Text.Json;

namespace MSBE.Client;

/// <summary>Typed RPC methods over an instance's state: its deployment journal, conflicts, updates and snapshots.</summary>
/// <remarks>
/// They answer busy while a job holds the instance state. Applying updates and restoring a snapshot
/// run only as jobs. See <c>docs/03-architecture.md</c> §3.4.
/// </remarks>
public static class InstanceStateRpc
{
    /// <summary>Updates a profile's mods from their providers, as a job.</summary>
    public const string UpdateApply = "update.apply";

    /// <summary>Restores an instance snapshot, as a job.</summary>
    public const string SnapshotRestore = "snapshot.restore";

    /// <summary>Lists an instance's deployments still in effect.</summary>
    /// <param name="client">The daemon client.</param>
    /// <param name="instance">The instance.</param>
    /// <param name="cancellationToken">Cancels the pending request.</param>
    /// <returns>The deployments, oldest first; the last is the one deployed.</returns>
    public static async Task<IReadOnlyList<JournalEntryInfo>> ListJournalAsync(this IMsbeClient client, string instance, CancellationToken cancellationToken)
    {
        ArgumentNullException.ThrowIfNull(client);
        JsonElement result = await client.InvokeAsync("journal.list", Instance(instance), cancellationToken).ConfigureAwait(false);
        return Journal(result);
    }

    /// <summary>Undoes every deployment after a transaction, so that it is in effect again.</summary>
    /// <param name="client">The daemon client.</param>
    /// <param name="instance">The instance.</param>
    /// <param name="transaction">The transaction to return to.</param>
    /// <param name="cancellationToken">Cancels the pending request.</param>
    /// <returns>What was undone, and the deployments left in effect.</returns>
    public static async Task<RollbackInfo> RollBackToAsync(this IMsbeClient client, string instance, long transaction, CancellationToken cancellationToken)
    {
        ArgumentNullException.ThrowIfNull(client);
        JsonElement result = await client.InvokeAsync(
            "journal.rollback",
            writer =>
            {
                writer.WriteStartObject();
                writer.WriteString("instance", instance);
                writer.WriteNumber("txn", transaction);
                writer.WriteEndObject();
            },
            cancellationToken).ConfigureAwait(false);
        return new RollbackInfo(
            [.. result.GetProperty("rolled_back").EnumerateArray().Select(undone => undone.GetInt64())],
            Journal(result.GetProperty("journal")));
    }

    /// <summary>Lists the paths more than one mod in a profile would place with different contents.</summary>
    /// <param name="client">The daemon client.</param>
    /// <param name="instance">The instance.</param>
    /// <param name="profile">The profile.</param>
    /// <param name="cancellationToken">Cancels the pending request.</param>
    /// <returns>The conflicting paths, in path order.</returns>
    public static async Task<IReadOnlyList<ConflictInfo>> ListConflictsAsync(this IMsbeClient client, string instance, string profile, CancellationToken cancellationToken)
    {
        ArgumentNullException.ThrowIfNull(client);
        JsonElement result = await client.InvokeAsync("conflicts.list", Profile(instance, profile), cancellationToken).ConfigureAwait(false);
        var conflicts = new List<ConflictInfo>();
        foreach (JsonElement conflict in result.EnumerateArray())
        {
            conflicts.Add(new ConflictInfo(
                Text(conflict, "path"),
                [.. conflict.GetProperty("claims").EnumerateArray().Select(claim => new ConflictClaimInfo(Text(claim, "module"), Text(claim, "blob")))]));
        }

        return conflicts;
    }

    /// <summary>Checks a profile's mods for updates without changing anything.</summary>
    /// <param name="client">The daemon client.</param>
    /// <param name="instance">The instance.</param>
    /// <param name="profile">The profile.</param>
    /// <param name="cancellationToken">Cancels the pending request.</param>
    /// <returns>What updating would do.</returns>
    public static async Task<UpdateReportInfo> PreviewUpdatesAsync(this IMsbeClient client, string instance, string profile, CancellationToken cancellationToken)
    {
        ArgumentNullException.ThrowIfNull(client);
        JsonElement result = await client.InvokeAsync("update.preview", Profile(instance, profile), cancellationToken).ConfigureAwait(false);
        return new UpdateReportInfo(
            result.TryGetProperty("dry_run", out JsonElement dryRun) && dryRun.ValueKind == JsonValueKind.True,
            [.. Items(result, "updated").Select(update => new ModUpdateInfo(Text(update, "module"), Text(update, "from"), Text(update, "to")))],
            Names(result, "current"),
            Names(result, "no_compatible_version"),
            Names(result, "unlisted"),
            Names(result, "not_updatable"),
            Requirements(result, "unresolved"),
            Requirements(result, "incompatible"));
    }

    /// <summary>Starts a job that updates a profile's mods from their providers.</summary>
    /// <param name="client">The daemon client.</param>
    /// <param name="instance">The instance.</param>
    /// <param name="profile">The profile.</param>
    /// <param name="cancellationToken">Cancels the pending request.</param>
    /// <returns>The job ID.</returns>
    public static Task<long> StartUpdateJobAsync(this IMsbeClient client, string instance, string profile, CancellationToken cancellationToken) =>
        StartJobAsync(client, UpdateApply, Profile(instance, profile), cancellationToken);

    /// <summary>Starts a job that restores an instance snapshot.</summary>
    /// <param name="client">The daemon client.</param>
    /// <param name="input">The snapshot's absolute path.</param>
    /// <param name="cancellationToken">Cancels the pending request.</param>
    /// <returns>The job ID.</returns>
    public static Task<long> StartSnapshotRestoreJobAsync(this IMsbeClient client, string input, CancellationToken cancellationToken) =>
        StartJobAsync(
            client,
            SnapshotRestore,
            writer =>
            {
                writer.WriteStartObject();
                writer.WriteString("input", input);
                writer.WriteEndObject();
            },
            cancellationToken);

    private static async Task<long> StartJobAsync(IMsbeClient client, string method, Action<Utf8JsonWriter> writeParameters, CancellationToken cancellationToken)
    {
        ArgumentNullException.ThrowIfNull(client);
        JsonElement result = await client.InvokeAsync(
            "job.start",
            writer =>
            {
                writer.WriteStartObject();
                writer.WriteString("method", method);
                writer.WritePropertyName("params");
                writeParameters(writer);
                writer.WriteEndObject();
            },
            cancellationToken).ConfigureAwait(false);
        return result.GetProperty("job_id").GetInt64();
    }

    private static Action<Utf8JsonWriter> Instance(string instance) => writer =>
    {
        writer.WriteStartObject();
        writer.WriteString("instance", instance);
        writer.WriteEndObject();
    };

    private static Action<Utf8JsonWriter> Profile(string instance, string profile) => writer =>
    {
        writer.WriteStartObject();
        writer.WriteString("instance", instance);
        writer.WriteString("profile", profile);
        writer.WriteEndObject();
    };

    private static List<JournalEntryInfo> Journal(JsonElement entries) =>
        [.. entries.EnumerateArray().Select(entry => new JournalEntryInfo(entry.GetProperty("txn").GetInt64(), Text(entry, "profile"), entry.GetProperty("files").GetInt64()))];

    private static List<JsonElement> Items(JsonElement element, string name) =>
        element.TryGetProperty(name, out JsonElement items) && items.ValueKind == JsonValueKind.Array ? [.. items.EnumerateArray()] : [];

    private static List<string> Names(JsonElement element, string name) => [.. Items(element, name).Select(item => item.GetString() ?? string.Empty)];

    private static List<RequirementInfo> Requirements(JsonElement element, string name) =>
        [.. Items(element, name).Select(item => new RequirementInfo(Text(item, "provider"), Text(item, "project_id"), Text(item, "declared_by")))];

    private static string Text(JsonElement element, string name) =>
        element.TryGetProperty(name, out JsonElement value) && value.ValueKind == JsonValueKind.String ? value.GetString() ?? string.Empty : string.Empty;
}
