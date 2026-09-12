using System.Text.Json;

namespace MSBE.Client;

/// <summary>Typed pack, job and snapshot RPC methods.</summary>
/// <remarks>
/// Formats and their options are discovered from the daemon, previews are held by the daemon under
/// a plan ID and digest, and execution runs only as a job. See
/// <c>docs/17-pack-formats-and-native-bundles.md</c> §17.13.
/// </remarks>
public static class PackRpc
{
    /// <summary>Runs a held import plan.</summary>
    public const string ImportExecute = "pack.import.execute";

    /// <summary>Runs a held update plan.</summary>
    public const string UpdateExecute = "pack.update.execute";

    /// <summary>Runs a held export plan.</summary>
    public const string ExportExecute = "pack.export.execute";

    /// <summary>Runs a held capture plan.</summary>
    public const string CaptureExecute = "pack.capture.execute";

    /// <summary>Lists codecs supporting a direction.</summary>
    /// <param name="client">The daemon client.</param>
    /// <param name="direction"><c>import</c> or <c>export</c>.</param>
    /// <param name="cancellationToken">Cancels the pending request.</param>
    /// <returns>The permitted codecs, in ID order.</returns>
    public static async Task<IReadOnlyList<PackCodecInfo>> ListPackCodecsAsync(this IMsbeClient client, string direction, CancellationToken cancellationToken)
    {
        ArgumentNullException.ThrowIfNull(client);
        JsonElement result = await client.InvokeAsync(
            "pack.codec.list",
            writer =>
            {
                writer.WriteStartObject();
                writer.WriteString("direction", direction);
                writer.WriteEndObject();
            },
            cancellationToken).ConfigureAwait(false);
        var codecs = new List<PackCodecInfo>();
        foreach (JsonElement codec in result.EnumerateArray())
        {
            JsonElement directions = codec.GetProperty("directions");
            codecs.Add(new PackCodecInfo(
                Text(codec, "id"),
                Text(codec, "name"),
                codec.GetProperty("extensions").EnumerateArray().Select(extension => extension.GetString() ?? string.Empty).ToArray(),
                directions.GetProperty("import").GetBoolean(),
                directions.GetProperty("export").GetBoolean(),
                !codec.TryGetProperty("provider", out JsonElement provider) || provider.ValueKind == JsonValueKind.Null));
        }

        return codecs;
    }

    /// <summary>Retrieves a codec's export option schema with a preset applied.</summary>
    /// <param name="client">The daemon client.</param>
    /// <param name="codec">The codec ID.</param>
    /// <param name="preset">The preset to apply, or <see langword="null" /> for defaults.</param>
    /// <param name="cancellationToken">Cancels the pending request.</param>
    /// <returns>The schema's fields, presets and normalized values.</returns>
    public static async Task<PackCodecOptions> GetPackOptionsAsync(this IMsbeClient client, string codec, string? preset, CancellationToken cancellationToken)
    {
        ArgumentNullException.ThrowIfNull(client);
        JsonElement result = await client.InvokeAsync(
            "pack.codec.options",
            writer =>
            {
                writer.WriteStartObject();
                writer.WriteString("codec", codec);
                writer.WriteString("direction", "export");
                if (preset is not null)
                {
                    writer.WriteString("preset", preset);
                }

                writer.WriteEndObject();
            },
            cancellationToken).ConfigureAwait(false);
        JsonElement schema = result.GetProperty("schema");
        var fields = new List<PackOptionField>();
        if (schema.TryGetProperty("fields", out JsonElement fieldElements))
        {
            foreach (JsonElement field in fieldElements.EnumerateArray())
            {
                JsonElement kind = field.GetProperty("kind");
                PackOptionChoice[] choices = kind.TryGetProperty("values", out JsonElement values)
                    ? values.EnumerateArray().Select(choice => new PackOptionChoice(Text(choice, "value"), Text(choice, "label"))).ToArray()
                    : [];
                fields.Add(new PackOptionField(Text(field, "key"), Text(field, "label"), Text(field, "description"), Text(kind, "kind"), choices));
            }
        }

        var presets = new List<PackPreset>();
        if (schema.TryGetProperty("presets", out JsonElement presetElements))
        {
            foreach (JsonElement item in presetElements.EnumerateArray())
            {
                presets.Add(new PackPreset(Text(item, "id"), Text(item, "name")));
            }
        }

        return new PackCodecOptions(Text(result, "codec"), Text(result, "name"), fields, presets, result.GetProperty("values"));
    }

    /// <summary>Previews an export and asks the daemon to hold the plan.</summary>
    /// <param name="client">The daemon client.</param>
    /// <param name="request">What to export.</param>
    /// <param name="cancellationToken">Cancels the pending request.</param>
    /// <returns>The held plan.</returns>
    public static Task<PackPlan> PreviewPackExportAsync(this IMsbeClient client, PackExportRequest request, CancellationToken cancellationToken)
    {
        ArgumentNullException.ThrowIfNull(request);
        return PreviewAsync(
            client,
            "pack.export.preview",
            writer =>
            {
                writer.WriteStartObject();
                writer.WriteString("instance", request.Instance);
                writer.WriteString("profile", request.Profile);
                writer.WriteString("codec", request.Codec);
                if (request.Preset is not null)
                {
                    writer.WriteString("preset", request.Preset);
                }

                writer.WriteString("output", request.Output);
                writer.WriteStartObject("options");
                foreach (KeyValuePair<string, JsonElement> option in request.Options)
                {
                    writer.WritePropertyName(option.Key);
                    option.Value.WriteTo(writer);
                }

                writer.WriteEndObject();
                writer.WriteEndObject();
            },
            cancellationToken);
    }

    /// <summary>Previews importing a pack into a new or empty profile.</summary>
    /// <param name="client">The daemon client.</param>
    /// <param name="instance">The instance.</param>
    /// <param name="profile">The profile to create or fill.</param>
    /// <param name="input">The absolute pack path.</param>
    /// <param name="cancellationToken">Cancels the pending request.</param>
    /// <returns>The held plan.</returns>
    public static Task<PackPlan> PreviewPackImportAsync(this IMsbeClient client, string instance, string profile, string input, CancellationToken cancellationToken) => PreviewAsync(
        client,
        "pack.import.preview",
        writer =>
        {
            writer.WriteStartObject();
            writer.WriteString("instance", instance);
            writer.WriteString("profile", profile);
            writer.WriteString("input", input);
            writer.WriteEndObject();
        },
        cancellationToken);

    /// <summary>Previews replacing a profile's pack layer and reapplying its changes.</summary>
    /// <param name="client">The daemon client.</param>
    /// <param name="instance">The instance.</param>
    /// <param name="profile">The profile.</param>
    /// <param name="input">The absolute path of the new pack version.</param>
    /// <param name="resolutions">Conflict resolutions, <c>keep</c> or <c>drop</c>, by conflict ID.</param>
    /// <param name="cancellationToken">Cancels the pending request.</param>
    /// <returns>The held plan.</returns>
    public static Task<PackPlan> PreviewPackUpdateAsync(this IMsbeClient client, string instance, string profile, string input, IReadOnlyDictionary<string, string> resolutions, CancellationToken cancellationToken)
    {
        ArgumentNullException.ThrowIfNull(resolutions);
        return PreviewAsync(
            client,
            "pack.update.preview",
            writer =>
            {
                writer.WriteStartObject();
                writer.WriteString("instance", instance);
                writer.WriteString("profile", profile);
                writer.WriteString("input", input);
                writer.WriteStartObject("resolutions");
                foreach (KeyValuePair<string, string> resolution in resolutions)
                {
                    writer.WriteString(resolution.Key, resolution.Value);
                }

                writer.WriteEndObject();
                writer.WriteEndObject();
            },
            cancellationToken);
    }

    /// <summary>Previews adopting in-game changes into the deployed profile.</summary>
    /// <param name="client">The daemon client.</param>
    /// <param name="instance">The instance.</param>
    /// <param name="profile">The deployed profile.</param>
    /// <param name="cancellationToken">Cancels the pending request.</param>
    /// <returns>The held plan.</returns>
    public static Task<PackPlan> PreviewPackCaptureAsync(this IMsbeClient client, string instance, string profile, CancellationToken cancellationToken) => PreviewAsync(
        client,
        "pack.capture.preview",
        writer =>
        {
            writer.WriteStartObject();
            writer.WriteString("instance", instance);
            writer.WriteString("profile", profile);
            writer.WriteEndObject();
        },
        cancellationToken);

    /// <summary>Starts a job that runs exactly a held plan.</summary>
    /// <param name="client">The daemon client.</param>
    /// <param name="executeMethod">The execute method for the plan's kind.</param>
    /// <param name="plan">The held plan.</param>
    /// <param name="cancellationToken">Cancels the pending request.</param>
    /// <returns>The job ID.</returns>
    public static async Task<long> StartPlanJobAsync(this IMsbeClient client, string executeMethod, PackPlan plan, CancellationToken cancellationToken)
    {
        ArgumentNullException.ThrowIfNull(client);
        ArgumentNullException.ThrowIfNull(plan);
        JsonElement result = await client.InvokeAsync(
            "job.start",
            writer =>
            {
                writer.WriteStartObject();
                writer.WriteString("method", executeMethod);
                writer.WriteStartObject("params");
                writer.WriteString("plan_id", plan.PlanId);
                writer.WriteString("plan_digest", plan.PlanDigest);
                writer.WriteEndObject();
                writer.WriteEndObject();
            },
            cancellationToken).ConfigureAwait(false);
        return result.GetProperty("job_id").GetInt64();
    }

    /// <summary>Reads a job's state and the events after a sequence.</summary>
    /// <param name="client">The daemon client.</param>
    /// <param name="jobId">The job.</param>
    /// <param name="after">The last event sequence already seen.</param>
    /// <param name="cancellationToken">Cancels the pending request.</param>
    /// <returns>The job's state and new events.</returns>
    public static async Task<JobStatusInfo> GetJobStatusAsync(this IMsbeClient client, long jobId, long after, CancellationToken cancellationToken)
    {
        ArgumentNullException.ThrowIfNull(client);
        JsonElement result = await client.InvokeAsync(
            "job.events",
            writer =>
            {
                writer.WriteStartObject();
                writer.WriteNumber("job_id", jobId);
                writer.WriteNumber("after", after);
                writer.WriteEndObject();
            },
            cancellationToken).ConfigureAwait(false);
        var events = new List<JobEventInfo>();
        foreach (JsonElement item in result.GetProperty("events").EnumerateArray())
        {
            events.Add(new JobEventInfo(
                item.GetProperty("sequence").GetInt64(),
                Text(item, "kind"),
                Text(item, "message"),
                Number(item, "completed"),
                Number(item, "total"),
                item.TryGetProperty("code", out JsonElement code) ? code.GetString() : null));
        }

        return new JobStatusInfo(result.GetProperty("job_id").GetInt64(), Text(result, "state"), events, result.GetProperty("next").GetInt64());
    }

    /// <summary>Asks a job to stop.</summary>
    /// <param name="client">The daemon client.</param>
    /// <param name="jobId">The job.</param>
    /// <param name="cancellationToken">Cancels the pending request.</param>
    /// <returns>Whether the request can still take effect.</returns>
    public static async Task<bool> CancelJobAsync(this IMsbeClient client, long jobId, CancellationToken cancellationToken)
    {
        ArgumentNullException.ThrowIfNull(client);
        JsonElement result = await client.InvokeAsync(
            "job.cancel",
            writer =>
            {
                writer.WriteStartObject();
                writer.WriteNumber("job_id", jobId);
                writer.WriteEndObject();
            },
            cancellationToken).ConfigureAwait(false);
        return result.GetProperty("cancelled").GetBoolean();
    }

    private static async Task<PackPlan> PreviewAsync(IMsbeClient client, string method, Action<Utf8JsonWriter> writeParameters, CancellationToken cancellationToken)
    {
        ArgumentNullException.ThrowIfNull(client);
        JsonElement result = await client.InvokeAsync(method, writeParameters, cancellationToken).ConfigureAwait(false);
        return new PackPlan(Text(result, "plan_id"), Text(result, "plan_digest"), result.GetProperty("plan"));
    }

    private static string Text(JsonElement element, string property) =>
        element.TryGetProperty(property, out JsonElement value) && value.ValueKind == JsonValueKind.String
            ? value.GetString() ?? string.Empty
            : string.Empty;

    private static long Number(JsonElement element, string property) =>
        element.TryGetProperty(property, out JsonElement value) && value.ValueKind == JsonValueKind.Number
            ? value.GetInt64()
            : 0;
}
