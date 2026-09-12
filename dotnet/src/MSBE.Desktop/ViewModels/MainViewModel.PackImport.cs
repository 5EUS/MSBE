using System.Collections.ObjectModel;
using System.Net.Sockets;
using System.Text.Json;

using CommunityToolkit.Mvvm.ComponentModel;
using CommunityToolkit.Mvvm.Input;

using MSBE.Client;

namespace MSBE.Desktop.ViewModels;

/// <content>Pack import into a new profile, and pack-layer updates with conflict resolution.</content>
internal sealed partial class MainViewModel
{
    private PackPlan? importPlan;

    /// <summary>Gets or sets the pack file to import or update from.</summary>
    [ObservableProperty]
    public partial string PackImportPath { get; set; } = string.Empty;

    /// <summary>Gets or sets the new profile an import creates.</summary>
    [ObservableProperty]
    public partial string PackImportProfile { get; set; } = "imported";

    /// <summary>Gets or sets a value indicating whether the shown preview is an update of the selected profile.</summary>
    [ObservableProperty]
    public partial bool IsPackUpdatePreview { get; set; }

    /// <summary>Gets or sets a value indicating whether an import or update preview is shown.</summary>
    [ObservableProperty]
    [NotifyPropertyChangedFor(nameof(CanExecuteImport))]
    public partial bool HasImportPreview { get; set; }

    /// <summary>Gets or sets a value indicating whether the import or update preview has blockers.</summary>
    [ObservableProperty]
    [NotifyPropertyChangedFor(nameof(CanExecuteImport))]
    public partial bool HasImportBlockers { get; set; }

    /// <summary>Gets what the previewed import or update obtains, and every change it reapplies.</summary>
    public ObservableCollection<PackPreviewItem> ImportPreviewItems { get; } = [];

    /// <summary>Gets the previewed blockers and warnings.</summary>
    public ObservableCollection<PackIssueItem> ImportIssues { get; } = [];

    /// <summary>Gets the profile changes the updated pack no longer applies cleanly to.</summary>
    public ObservableCollection<PackConflictItem> ImportConflicts { get; } = [];

    /// <summary>Gets a value indicating whether the previewed import or update can run.</summary>
    public bool CanExecuteImport => this.HasImportPreview && !this.HasImportBlockers && !this.IsPackJobRunning;

    private static string DescribeAction(string action) => action switch
    {
        "in-store" => "Already stored",
        "embedded" => "Embedded",
        "acquire" => "Download",
        "user-action" => "User action",
        "reuse" => "Unchanged",
        "derive" => "Derived",
        "missing" => "Missing",
        _ => action,
    };

    [RelayCommand]
    private Task PreviewImportAsync() => this.PreviewPackLayerAsync(isUpdate: false);

    [RelayCommand]
    private Task PreviewUpdateAsync() => this.PreviewPackLayerAsync(isUpdate: true);

    [RelayCommand]
    private async Task ExecuteImportAsync()
    {
        if (this.importPlan is null || !this.CanExecuteImport || this.SelectedInstance is null)
        {
            return;
        }

        bool isUpdate = this.IsPackUpdatePreview;
        string instance = this.SelectedInstance;
        string target = isUpdate ? this.SelectedProfile ?? string.Empty : this.PackImportProfile.Trim();
        this.PackError = string.Empty;
        try
        {
            await this.RunPackJobAsync(isUpdate ? PackRpc.UpdateExecute : PackRpc.ImportExecute, this.importPlan).ConfigureAwait(true);
            this.ClearImportPreview();
            await this.LoadSelectedInstanceAsync(instance).ConfigureAwait(true);
            this.StatusMessage = isUpdate ? $"Updated the pack layer of {target}." : $"Imported the pack into {target}.";
        }
        catch (Exception exception) when (exception is IOException or SocketException or JsonException or InvalidOperationException)
        {
            this.PackError = exception.Message;
        }
    }

    private async Task PreviewPackLayerAsync(bool isUpdate)
    {
        if (this.SelectedInstance is null || this.SelectedProfile is null || string.IsNullOrWhiteSpace(this.PackImportPath) || this.IsPackBusy)
        {
            return;
        }

        string input = this.PackImportPath.Trim();
        var resolutions = this.ImportConflicts
            .Where(conflict => conflict.Resolution.Length > 0)
            .ToDictionary(conflict => conflict.Id, conflict => conflict.Resolution, StringComparer.Ordinal);
        this.IsPackBusy = true;
        this.PackError = string.Empty;
        this.ClearImportPreview();
        try
        {
            PackPlan plan = isUpdate
                ? await this.client.PreviewPackUpdateAsync(this.SelectedInstance, this.SelectedProfile, input, resolutions, CancellationToken.None).ConfigureAwait(true)
                : await this.client.PreviewPackImportAsync(this.SelectedInstance, this.PackImportProfile.Trim(), input, CancellationToken.None).ConfigureAwait(true);
            this.ShowImportPreview(plan, isUpdate);
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

    private void ShowImportPreview(PackPlan plan, bool isUpdate)
    {
        this.importPlan = plan;
        this.IsPackUpdatePreview = isUpdate;
        JsonElement preview = plan.Plan;
        foreach (JsonElement item in preview.GetProperty("items").EnumerateArray())
        {
            this.ImportPreviewItems.Add(new PackPreviewItem(DescribeAction(Text(item, "action")), Text(item, "subject")));
        }

        if (isUpdate)
        {
            foreach (JsonElement change in preview.GetProperty("changes").EnumerateArray())
            {
                string subject = Text(change, "subject");
                this.ImportPreviewItems.Add(new PackPreviewItem("Reapply change", subject.Length == 0 ? Text(change, "kind") : $"{Text(change, "kind")} {subject}"));
            }

            foreach (JsonElement conflict in preview.GetProperty("conflicts").EnumerateArray())
            {
                string? resolution = conflict.TryGetProperty("resolution", out JsonElement chosen) ? chosen.GetString() : null;
                this.ImportConflicts.Add(new PackConflictItem(Text(conflict, "id"), Text(conflict, "reason"), resolution));
            }
        }

        AddIssues(this.ImportIssues, preview);
        this.HasImportBlockers = this.ImportIssues.Any(issue => issue.IsBlocker);
        this.HasImportPreview = true;
    }

    private void ClearImportPreview()
    {
        this.importPlan = null;
        this.ImportPreviewItems.Clear();
        this.ImportIssues.Clear();
        this.ImportConflicts.Clear();
        this.HasImportBlockers = false;
        this.HasImportPreview = false;
    }
}
