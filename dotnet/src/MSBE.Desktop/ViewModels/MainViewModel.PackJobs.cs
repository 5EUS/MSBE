using System.Collections.ObjectModel;
using System.Net.Sockets;
using System.Text.Json;

using CommunityToolkit.Mvvm.ComponentModel;
using CommunityToolkit.Mvvm.Input;

using MSBE.Client;

namespace MSBE.Desktop.ViewModels;

/// <content>Daemon jobs that run held pack plans, with progress and cancellation.</content>
internal sealed partial class MainViewModel
{
    private static readonly TimeSpan PackJobPollInterval = TimeSpan.FromMilliseconds(250);

    private long? activePackJob;

    /// <summary>Gets or sets a value indicating whether a pack job is running.</summary>
    [ObservableProperty]
    [NotifyPropertyChangedFor(nameof(CanExecuteExport))]
    [NotifyPropertyChangedFor(nameof(CanExecuteImport))]
    [NotifyPropertyChangedFor(nameof(CanExecuteCapture))]
    public partial bool IsPackJobRunning { get; set; }

    /// <summary>Gets or sets the running job's completed fraction, from zero to one.</summary>
    [ObservableProperty]
    public partial double PackJobProgress { get; set; }

    /// <summary>Gets or sets what the running job reports it is doing.</summary>
    [ObservableProperty]
    public partial string PackJobMessage { get; set; } = string.Empty;

    /// <summary>Gets or sets a value indicating whether the daemon serves typed pack RPC.</summary>
    [ObservableProperty]
    public partial bool IsTypedPackSupported { get; set; }

    private static string Text(JsonElement element, string property) =>
        element.TryGetProperty(property, out JsonElement value) && value.ValueKind == JsonValueKind.String
            ? value.GetString() ?? string.Empty
            : string.Empty;

    private static void AddIssues(ObservableCollection<PackIssueItem> issues, JsonElement preview)
    {
        foreach (JsonElement blocker in preview.GetProperty("blockers").EnumerateArray())
        {
            issues.Add(new PackIssueItem(true, Text(blocker, "code"), Text(blocker, "message")));
        }

        foreach (JsonElement warning in preview.GetProperty("warnings").EnumerateArray())
        {
            issues.Add(new PackIssueItem(false, Text(warning, "code"), Text(warning, "message")));
        }
    }

    [RelayCommand]
    private async Task CancelPackJobAsync()
    {
        if (this.activePackJob is not long job)
        {
            return;
        }

        try
        {
            await this.client.CancelJobAsync(job, CancellationToken.None).ConfigureAwait(true);
            this.PackJobMessage = "Cancelling";
        }
        catch (Exception exception) when (exception is IOException or SocketException or JsonException or InvalidOperationException)
        {
            this.PackError = exception.Message;
        }
    }

    /// <summary>Runs a held plan as a daemon job and follows it until it finishes.</summary>
    /// <param name="method">The execute method for the plan's kind.</param>
    /// <param name="plan">The held plan.</param>
    /// <returns>A task that completes when the job succeeds.</returns>
    /// <exception cref="InvalidOperationException">The job failed or was cancelled.</exception>
    private async Task RunPackJobAsync(string method, PackPlan plan)
    {
        long job = await this.client.StartPlanJobAsync(method, plan, CancellationToken.None).ConfigureAwait(true);
        this.activePackJob = job;
        this.IsPackJobRunning = true;
        this.PackJobProgress = 0;
        this.PackJobMessage = "Queued";
        try
        {
            long after = 0;
            JobEventInfo? last = null;
            while (true)
            {
                JobStatusInfo status = await this.client.GetJobStatusAsync(job, after, CancellationToken.None).ConfigureAwait(true);
                after = status.Next;
                foreach (JobEventInfo item in status.Events)
                {
                    last = item;
                    if (string.Equals(item.Kind, "progress", StringComparison.Ordinal))
                    {
                        this.PackJobMessage = item.Message;
                        this.PackJobProgress = item.Total == 0 ? 0 : (double)item.Completed / item.Total;
                    }
                }

                if (status.IsFinished)
                {
                    if (!string.Equals(status.State, "succeeded", StringComparison.Ordinal))
                    {
                        throw new InvalidOperationException(last is { Message.Length: > 0 } ? last.Message : $"The job ended as {status.State}.");
                    }

                    return;
                }

                await Task.Delay(PackJobPollInterval).ConfigureAwait(true);
            }
        }
        finally
        {
            this.activePackJob = null;
            this.IsPackJobRunning = false;
            this.PackJobProgress = 0;
            this.PackJobMessage = string.Empty;
        }
    }
}
