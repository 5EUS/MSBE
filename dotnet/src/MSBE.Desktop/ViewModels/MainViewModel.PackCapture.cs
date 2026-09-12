using System.Collections.ObjectModel;
using System.Net.Sockets;
using System.Text;
using System.Text.Json;

using CommunityToolkit.Mvvm.ComponentModel;
using CommunityToolkit.Mvvm.Input;

using MSBE.Client;

namespace MSBE.Desktop.ViewModels;

/// <content>Capturing in-game changes beneath the plan's mutable roots into the deployed profile.</content>
internal sealed partial class MainViewModel
{
    private PackPlan? capturePlan;

    /// <summary>Gets the files a capture would adopt, with their diffs.</summary>
    public ObservableCollection<PackCaptureItem> CaptureItems { get; } = [];

    /// <summary>Gets or sets a value indicating whether a capture preview is shown.</summary>
    [ObservableProperty]
    [NotifyPropertyChangedFor(nameof(CanExecuteCapture))]
    public partial bool HasCapturePreview { get; set; }

    /// <summary>Gets a value indicating whether the previewed capture can run.</summary>
    public bool CanExecuteCapture => this.HasCapturePreview && this.CaptureItems.Count > 0 && !this.IsPackJobRunning;

    private static string DescribeDiff(JsonElement item)
    {
        if (!item.TryGetProperty("diff", out JsonElement diff) || diff.ValueKind != JsonValueKind.Array)
        {
            return string.Empty;
        }

        var text = new StringBuilder();
        foreach (JsonElement line in diff.EnumerateArray())
        {
            char marker = Text(line, "kind") switch
            {
                "added" => '+',
                "removed" => '-',
                _ => ' ',
            };
            text.Append(marker).Append(Text(line, "text")).Append('\n');
        }

        return text.ToString();
    }

    [RelayCommand]
    private async Task PreviewCaptureAsync()
    {
        if (this.SelectedInstance is null || this.SelectedProfile is null || this.IsPackBusy)
        {
            return;
        }

        this.IsPackBusy = true;
        this.PackError = string.Empty;
        this.ClearCapturePreview();
        try
        {
            PackPlan plan = await this.client.PreviewPackCaptureAsync(this.SelectedInstance, this.SelectedProfile, CancellationToken.None).ConfigureAwait(true);
            this.capturePlan = plan;
            foreach (JsonElement item in plan.Plan.GetProperty("items").EnumerateArray())
            {
                this.CaptureItems.Add(new PackCaptureItem(Text(item, "path"), Text(item, "kind"), DescribeDiff(item)));
            }

            this.HasCapturePreview = true;
            if (this.CaptureItems.Count == 0)
            {
                this.StatusMessage = "No in-game changes to capture.";
            }
        }
        catch (Exception exception) when (exception is IOException or SocketException or JsonException or InvalidOperationException or KeyNotFoundException)
        {
            this.PackError = exception.Message;
        }
        finally
        {
            this.IsPackBusy = false;
        }
    }

    [RelayCommand]
    private async Task ExecuteCaptureAsync()
    {
        if (this.capturePlan is null || !this.CanExecuteCapture || this.SelectedInstance is null || this.SelectedProfile is null)
        {
            return;
        }

        string instance = this.SelectedInstance;
        string profile = this.SelectedProfile;
        int count = this.CaptureItems.Count;
        this.PackError = string.Empty;
        try
        {
            await this.RunPackJobAsync(PackRpc.CaptureExecute, this.capturePlan).ConfigureAwait(true);
            this.ClearCapturePreview();
            this.StatusMessage = $"Captured {count} file(s) into {profile}.";
            await this.LoadPackConfigsAsync(instance, profile).ConfigureAwait(true);
        }
        catch (Exception exception) when (exception is IOException or SocketException or JsonException or InvalidOperationException)
        {
            this.PackError = exception.Message;
        }
    }

    private void ClearCapturePreview()
    {
        this.capturePlan = null;
        this.CaptureItems.Clear();
        this.HasCapturePreview = false;
    }
}
