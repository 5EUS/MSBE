using System.Collections.ObjectModel;
using System.Globalization;
using System.Net.Sockets;
using System.Text.Json;

using CommunityToolkit.Mvvm.ComponentModel;
using CommunityToolkit.Mvvm.Input;

using MSBE.Client;
using MSBE.Desktop.Resources;

namespace MSBE.Desktop.ViewModels;

/// <content>Codec-driven export: discovered formats, schema-rendered options, previews and jobs.</content>
internal sealed partial class MainViewModel
{
    private PackPlan? exportPlan;
    private bool isLoadingExportOptions;

    /// <summary>Gets the export codecs the daemon discovered.</summary>
    public ObservableCollection<PackCodecInfo> ExportCodecs { get; } = [];

    /// <summary>Gets or sets the codec to export with.</summary>
    [ObservableProperty]
    public partial PackCodecInfo? SelectedExportCodec { get; set; }

    /// <summary>Gets the presets the selected codec's schema offers.</summary>
    public ObservableCollection<PackPreset> ExportPresets { get; } = [];

    /// <summary>Gets or sets the preset the options start from.</summary>
    [ObservableProperty]
    public partial PackPreset? SelectedExportPreset { get; set; }

    /// <summary>Gets the selected codec's options, rendered from its schema.</summary>
    public ObservableCollection<PackOptionItem> ExportOptions { get; } = [];

    /// <summary>Gets or sets the export destination.</summary>
    [ObservableProperty]
    public partial string PackOutputPath { get; set; } = string.Empty;

    /// <summary>Gets the previewed files, grouped by how the export carries them.</summary>
    public ObservableCollection<PackPreviewItem> ExportPreviewItems { get; } = [];

    /// <summary>Gets the previewed blockers and warnings.</summary>
    public ObservableCollection<PackIssueItem> ExportIssues { get; } = [];

    /// <summary>Gets the observations the preview relied on, with their ages.</summary>
    public ObservableCollection<string> ExportObservations { get; } = [];

    /// <summary>Gets or sets the preview's totals.</summary>
    [ObservableProperty]
    public partial string ExportSummary { get; set; } = string.Empty;

    /// <summary>Gets or sets a value indicating whether an export preview is shown.</summary>
    [ObservableProperty]
    [NotifyPropertyChangedFor(nameof(CanExecuteExport))]
    public partial bool HasExportPreview { get; set; }

    /// <summary>Gets or sets a value indicating whether the export preview has blockers.</summary>
    [ObservableProperty]
    [NotifyPropertyChangedFor(nameof(CanExecuteExport))]
    public partial bool HasExportBlockers { get; set; }

    /// <summary>Gets a value indicating whether the previewed export can run.</summary>
    public bool CanExecuteExport => this.HasExportPreview && !this.HasExportBlockers && !this.IsPackJobRunning;

    private static string DescribeGroup(string group) => group switch
    {
        "provider-reference" => Strings.PackGroupProviderReference,
        "user-action" => Strings.PackGroupUserAction,
        "environment-input" => Strings.PackGroupEnvironmentInput,
        "embedded-config" => Strings.PackGroupEmbeddedConfig,
        "embedded-local" => Strings.PackGroupEmbeddedLocal,
        "embedded-other" => Strings.PackGroupEmbeddedOther,
        "derived" => Strings.PackGroupDerived,
        "policy-blocker" => Strings.PackGroupPolicyBlocker,
        "omitted" => Strings.PackGroupOmitted,
        _ => group,
    };

    partial void OnSelectedExportCodecChanged(PackCodecInfo? value)
    {
        this.ClearExportPreview();
        if (value is null)
        {
            this.ExportOptions.Clear();
            this.ExportPresets.Clear();
            return;
        }

        if (this.SelectedInstance is not null && this.SelectedProfile is not null)
        {
            this.SetDefaultPackOutputPath(this.SelectedInstance, this.SelectedProfile);
        }

        _ = this.LoadExportOptionsAsync(value.Id, preset: null);
    }

    partial void OnSelectedExportPresetChanged(PackPreset? value)
    {
        if (!this.isLoadingExportOptions && value is not null && this.SelectedExportCodec is not null)
        {
            _ = this.LoadExportOptionsAsync(this.SelectedExportCodec.Id, value.Id);
        }
    }

    [RelayCommand]
    private async Task LoadExportCodecsAsync()
    {
        if (!this.IsTypedPackSupported)
        {
            return;
        }

        try
        {
            IReadOnlyList<PackCodecInfo> codecs = await this.client.ListPackCodecsAsync("export", CancellationToken.None).ConfigureAwait(true);
            string? selected = this.SelectedExportCodec?.Id;
            this.ExportCodecs.Clear();
            foreach (PackCodecInfo codec in codecs)
            {
                this.ExportCodecs.Add(codec);
            }

            this.SelectedExportCodec = this.ExportCodecs.FirstOrDefault(codec => string.Equals(codec.Id, selected, StringComparison.Ordinal))
                ?? this.ExportCodecs.FirstOrDefault(codec => codec.IsProviderNeutral)
                ?? this.ExportCodecs.FirstOrDefault();
        }
        catch (Exception exception) when (exception is IOException or SocketException or JsonException or InvalidOperationException)
        {
            this.PackError = exception.Message;
        }
    }

    private async Task LoadExportOptionsAsync(string codec, string? preset)
    {
        try
        {
            PackCodecOptions options = await this.client.GetPackOptionsAsync(codec, preset, CancellationToken.None).ConfigureAwait(true);
            if (!string.Equals(this.SelectedExportCodec?.Id, codec, StringComparison.Ordinal))
            {
                return;
            }

            this.isLoadingExportOptions = true;
            try
            {
                this.ExportPresets.Clear();
                foreach (PackPreset item in options.Presets)
                {
                    this.ExportPresets.Add(item);
                }

                this.SelectedExportPreset = this.ExportPresets.FirstOrDefault(item => string.Equals(item.Id, preset, StringComparison.Ordinal));
                this.ExportOptions.Clear();
                foreach (PackOptionField field in options.Fields)
                {
                    JsonElement value = options.Values.TryGetProperty(field.Key, out JsonElement found) ? found : default;
                    this.ExportOptions.Add(new PackOptionItem(field, value));
                }
            }
            finally
            {
                this.isLoadingExportOptions = false;
            }

            this.ClearExportPreview();
        }
        catch (Exception exception) when (exception is IOException or SocketException or JsonException or InvalidOperationException)
        {
            this.PackError = exception.Message;
        }
    }

    [RelayCommand]
    private async Task PreviewExportAsync()
    {
        if (this.SelectedInstance is null || this.SelectedProfile is null || this.SelectedExportCodec is null || string.IsNullOrWhiteSpace(this.PackOutputPath) || this.IsPackBusy)
        {
            return;
        }

        this.IsPackBusy = true;
        this.PackError = string.Empty;
        this.ClearExportPreview();
        try
        {
            var options = this.ExportOptions.ToDictionary(option => option.Key, option => option.ToJson(), StringComparer.Ordinal);
            var request = new PackExportRequest(this.SelectedInstance, this.SelectedProfile, this.SelectedExportCodec.Id, this.SelectedExportPreset?.Id, options, this.PackOutputPath.Trim());
            PackPlan plan = await this.client.PreviewPackExportAsync(request, CancellationToken.None).ConfigureAwait(true);
            this.ShowExportPreview(plan);
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
    private async Task ExecuteExportAsync()
    {
        if (this.exportPlan is null || !this.CanExecuteExport)
        {
            return;
        }

        string output = this.PackOutputPath.Trim();
        this.PackError = string.Empty;
        try
        {
            await this.RunPackJobAsync(PackRpc.ExportExecute, this.exportPlan).ConfigureAwait(true);
            this.ClearExportPreview();
            this.StatusMessage = Strings.FormatPackExported(this.SelectedProfile, output);
        }
        catch (Exception exception) when (exception is IOException or SocketException or JsonException or InvalidOperationException)
        {
            this.PackError = exception.Message;
        }
    }

    private void ShowExportPreview(PackPlan plan)
    {
        this.exportPlan = plan;
        JsonElement preview = plan.Plan;
        foreach (JsonElement item in preview.GetProperty("items").EnumerateArray())
        {
            this.ExportPreviewItems.Add(new PackPreviewItem(DescribeGroup(Text(item, "group")), Text(item, "path")));
        }

        AddIssues(this.ExportIssues, preview);
        foreach (JsonElement observation in preview.GetProperty("observations").EnumerateArray())
        {
            this.ExportObservations.Add(this.DescribeObservation(Text(observation, "subject"), Text(observation, "observed_at")));
        }

        int requirements = preview.GetProperty("requirements").GetArrayLength();
        int environment = preview.GetProperty("environment").GetArrayLength();
        long bytes = preview.GetProperty("embedded_bytes").GetInt64();
        this.ExportSummary = Strings.FormatPackExportSummary(requirements, environment, bytes.ToString("N0", CultureInfo.InvariantCulture));
        this.HasExportBlockers = this.ExportIssues.Any(issue => issue.IsBlocker);
        this.HasExportPreview = true;
    }

    private string DescribeObservation(string subject, string observedAt)
    {
        if (!DateOnly.TryParse(observedAt, CultureInfo.InvariantCulture, DateTimeStyles.None, out DateOnly observed))
        {
            return Strings.FormatPackObservedAt(subject, observedAt);
        }

        int days = DateOnly.FromDateTime(this.time.GetUtcNow().UtcDateTime).DayNumber - observed.DayNumber;
        return days <= 0
            ? Strings.FormatPackObservedToday(subject)
            : Strings.FormatPackObservedDaysAgo(subject, days);
    }

    private void ClearExportPreview()
    {
        this.exportPlan = null;
        this.ExportPreviewItems.Clear();
        this.ExportIssues.Clear();
        this.ExportObservations.Clear();
        this.ExportSummary = string.Empty;
        this.HasExportBlockers = false;
        this.HasExportPreview = false;
    }
}
