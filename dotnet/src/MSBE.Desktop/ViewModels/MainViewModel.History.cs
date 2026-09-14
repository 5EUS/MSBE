using System.Collections.ObjectModel;
using System.Net.Sockets;
using System.Text.Json;

using CommunityToolkit.Mvvm.ComponentModel;
using CommunityToolkit.Mvvm.Input;

using MSBE.Client;
using MSBE.Desktop.Resources;

namespace MSBE.Desktop.ViewModels;

/// <content>
/// The History workspace: the deployments still in effect with rollback to any of them, the selected
/// profile's conflicts with the action that resolves each, provider updates, and snapshot restore.
/// </content>
internal sealed partial class MainViewModel
{
    /// <summary>Gets the deployments still in effect on the selected instance, newest first.</summary>
    public ObservableCollection<JournalItem> JournalEntries { get; } = [];

    /// <summary>Gets the paths more than one mod in the selected profile would place.</summary>
    public ObservableCollection<ConflictItem> Conflicts { get; } = [];

    /// <summary>Gets the mods in the selected profile with a newer compatible release.</summary>
    public ObservableCollection<UpdateItem> AvailableUpdates { get; } = [];

    /// <summary>Gets or sets why no deployment is listed, or empty when they are.</summary>
    [ObservableProperty]
    public partial string JournalStatus { get; set; } = Strings.JournalNoInstance;

    /// <summary>Gets or sets the deployment the user asked to roll back to, until they confirm or cancel.</summary>
    [ObservableProperty]
    [NotifyPropertyChangedFor(nameof(HasPendingRollback))]
    public partial JournalItem? PendingRollback { get; set; }

    /// <summary>Gets or sets the question asked before rolling back.</summary>
    [ObservableProperty]
    public partial string PendingRollbackText { get; set; } = string.Empty;

    /// <summary>Gets or sets a summary of the conflicts, or why they could not be read.</summary>
    [ObservableProperty]
    public partial string ConflictsStatus { get; set; } = string.Empty;

    /// <summary>Gets or sets what the last update check found.</summary>
    [ObservableProperty]
    public partial string UpdatesStatus { get; set; } = Strings.UpdatesNotChecked;

    /// <summary>Gets or sets how the other mods stand after an update check.</summary>
    [ObservableProperty]
    [NotifyPropertyChangedFor(nameof(HasUpdatesDetail))]
    public partial string UpdatesDetail { get; set; } = string.Empty;

    /// <summary>Gets or sets the requirements and incompatibilities an update check could not settle.</summary>
    [ObservableProperty]
    [NotifyPropertyChangedFor(nameof(HasUpdatesProblems))]
    public partial string UpdatesProblems { get; set; } = string.Empty;

    /// <summary>Gets or sets the absolute path of the snapshot to restore.</summary>
    [ObservableProperty]
    [NotifyPropertyChangedFor(nameof(CanRestoreSnapshot))]
    public partial string SnapshotRestorePath { get; set; } = string.Empty;

    /// <summary>Gets or sets a value indicating whether MSBE asks before restoring the snapshot.</summary>
    [ObservableProperty]
    public partial bool IsConfirmingSnapshotRestore { get; set; }

    /// <summary>Gets or sets the question asked before restoring the snapshot.</summary>
    [ObservableProperty]
    public partial string SnapshotRestoreText { get; set; } = string.Empty;

    /// <summary>Gets a value indicating whether a rollback waits for confirmation.</summary>
    public bool HasPendingRollback => this.PendingRollback is not null;

    /// <summary>Gets a value indicating whether an update check described the other mods.</summary>
    public bool HasUpdatesDetail => this.UpdatesDetail.Length > 0;

    /// <summary>Gets a value indicating whether an update check found requirements it could not settle.</summary>
    public bool HasUpdatesProblems => this.UpdatesProblems.Length > 0;

    /// <summary>Gets a value indicating whether found updates can be applied now.</summary>
    public bool CanApplyUpdates => this.AvailableUpdates.Count > 0 && !this.IsPackJobRunning;

    /// <summary>Gets a value indicating whether a snapshot path is given and no job is running.</summary>
    public bool CanRestoreSnapshot => Path.IsPathFullyQualified(this.SnapshotRestorePath.Trim()) && !this.IsPackJobRunning;

    [RelayCommand]
    private async Task LoadHistoryAsync()
    {
        this.PendingRollback = null;
        if (this.SelectedInstance is not { } instance)
        {
            this.JournalEntries.Clear();
            this.Conflicts.Clear();
            this.JournalStatus = Strings.JournalNoInstance;
            this.ConflictsStatus = string.Empty;
            return;
        }

        await this.LoadJournalAsync(instance).ConfigureAwait(true);
        if (this.SelectedProfile is { } profile)
        {
            await this.LoadConflictsAsync(instance, profile).ConfigureAwait(true);
        }
    }

    private async Task LoadJournalAsync(string instance)
    {
        try
        {
            this.ShowJournal(instance, await this.client.ListJournalAsync(instance, CancellationToken.None).ConfigureAwait(true));
        }
        catch (MsbeRpcException exception) when (exception.Code == MethodNotFoundCode)
        {
            this.JournalEntries.Clear();
            this.JournalStatus = Strings.HistoryUnsupported;
        }
        catch (Exception exception) when (exception is IOException or SocketException or JsonException or InvalidOperationException or KeyNotFoundException)
        {
            this.JournalEntries.Clear();
            this.JournalStatus = Strings.FormatJournalFailed(exception.Message);
        }
    }

    /// <summary>Shows the deployments still in effect, newest first.</summary>
    private void ShowJournal(string instance, IReadOnlyList<JournalEntryInfo> journal)
    {
        this.JournalEntries.Clear();
        for (int index = journal.Count - 1; index >= 0; index--)
        {
            this.JournalEntries.Add(new JournalItem(journal[index], isDeployed: index == journal.Count - 1));
        }

        this.JournalStatus = journal.Count == 0 ? Strings.FormatJournalEmpty(instance) : string.Empty;
    }

    [RelayCommand]
    private void RequestRollbackTo(JournalItem? entry)
    {
        if (entry is not { IsDeployed: false })
        {
            return;
        }

        // Newest first, so every entry above this one is a later deployment the rollback undoes.
        this.PendingRollbackText = Strings.FormatJournalConfirmRollback(this.JournalEntries.IndexOf(entry), entry.Transaction);
        this.PendingRollback = entry;
    }

    [RelayCommand]
    private void CancelRollback() => this.PendingRollback = null;

    [RelayCommand]
    private async Task ConfirmRollbackAsync()
    {
        if (this.PendingRollback is not { } entry || this.SelectedInstance is not { } instance)
        {
            return;
        }

        this.PendingRollback = null;
        try
        {
            RollbackInfo rolledBack = await this.client.RollBackToAsync(instance, entry.Transaction, CancellationToken.None).ConfigureAwait(true);
            this.ShowJournal(instance, rolledBack.Journal);
            this.StatusMessage = Strings.FormatJournalRolledBack(entry.Transaction, rolledBack.RolledBack.Count);
        }
        catch (Exception exception) when (exception is IOException or SocketException or JsonException or InvalidOperationException or KeyNotFoundException)
        {
            this.JournalStatus = Strings.FormatJournalRollbackFailed(exception.Message);
            return;
        }

        await this.LoadSelectedInstanceAsync(instance).ConfigureAwait(true);
        await this.LoadProfilesAsync(instance).ConfigureAwait(true);
    }

    private async Task LoadConflictsAsync(string instance, string profile)
    {
        try
        {
            IReadOnlyList<ConflictInfo> conflicts = await this.client.ListConflictsAsync(instance, profile, CancellationToken.None).ConfigureAwait(true);
            this.Conflicts.Clear();
            foreach (ConflictInfo conflict in conflicts)
            {
                this.Conflicts.Add(new ConflictItem(conflict));
            }

            this.ConflictsStatus = conflicts.Count == 0 ? Strings.FormatConflictsNone(profile) : Strings.FormatConflictsSummary(conflicts.Count);
        }
        catch (MsbeRpcException exception) when (exception.Code == MethodNotFoundCode)
        {
            this.Conflicts.Clear();
            this.ConflictsStatus = Strings.HistoryUnsupported;
        }
        catch (Exception exception) when (exception is IOException or SocketException or JsonException or InvalidOperationException or KeyNotFoundException)
        {
            this.Conflicts.Clear();
            this.ConflictsStatus = Strings.FormatConflictsFailed(exception.Message);
        }
    }

    [RelayCommand]
    private async Task RemoveConflictingModAsync(ConflictClaimItem? claim)
    {
        if (claim is null || this.SelectedInstance is not { } instance || this.SelectedProfile is not { } profile)
        {
            return;
        }

        try
        {
            CommandResult result = await this.client.RunCommandAsync(
                ["--format", "json", "remove", instance, claim.Module, "--profile", profile],
                CancellationToken.None).ConfigureAwait(true);
            if (result.ExitCode != 0)
            {
                throw new InvalidOperationException(result.StandardError.Trim());
            }

            this.StatusMessage = Strings.FormatConflictRemovedStatus(claim.Module, profile);
        }
        catch (Exception exception) when (exception is IOException or SocketException or InvalidOperationException)
        {
            this.ConflictsStatus = Strings.FormatConflictRemoveFailed(claim.Module, exception.Message);
            return;
        }

        await this.LoadModsAsync(instance, profile).ConfigureAwait(true);
        await this.LoadConflictsAsync(instance, profile).ConfigureAwait(true);
    }

    [RelayCommand]
    private async Task CheckUpdatesAsync()
    {
        if (this.SelectedInstance is not { } instance || this.SelectedProfile is not { } profile)
        {
            return;
        }

        this.AvailableUpdates.Clear();
        this.UpdatesDetail = string.Empty;
        this.UpdatesProblems = string.Empty;
        try
        {
            UpdateReportInfo report = await this.client.PreviewUpdatesAsync(instance, profile, CancellationToken.None).ConfigureAwait(true);
            foreach (ModUpdateInfo update in report.Updated)
            {
                this.AvailableUpdates.Add(new UpdateItem(update));
            }

            this.UpdatesStatus = report.Updated.Count == 0
                ? Strings.FormatUpdatesNone(profile)
                : Strings.FormatUpdatesAvailable(report.Updated.Count, profile);
            this.UpdatesDetail = Strings.FormatUpdatesOther(report.Current.Count, report.NoCompatibleVersion.Count, report.Unlisted.Count, report.NotUpdatable.Count);
            this.UpdatesProblems = report.Unresolved.Count + report.Incompatible.Count == 0
                ? string.Empty
                : Strings.FormatUpdatesProblems(report.Unresolved.Count, report.Incompatible.Count);
        }
        catch (MsbeRpcException exception) when (exception.Code == MethodNotFoundCode)
        {
            this.UpdatesStatus = Strings.HistoryUnsupported;
        }
        catch (Exception exception) when (exception is IOException or SocketException or JsonException or InvalidOperationException or KeyNotFoundException)
        {
            this.UpdatesStatus = Strings.FormatUpdatesFailed(exception.Message);
        }
        finally
        {
            this.OnPropertyChanged(nameof(this.CanApplyUpdates));
        }
    }

    [RelayCommand]
    private async Task ApplyUpdatesAsync()
    {
        if (this.SelectedInstance is not { } instance || this.SelectedProfile is not { } profile || !this.CanApplyUpdates)
        {
            return;
        }

        int count = this.AvailableUpdates.Count;
        try
        {
            long job = await this.client.StartUpdateJobAsync(instance, profile, CancellationToken.None).ConfigureAwait(true);
            await this.FollowJobAsync(job).ConfigureAwait(true);
        }
        catch (Exception exception) when (exception is IOException or SocketException or JsonException or InvalidOperationException or KeyNotFoundException)
        {
            this.UpdatesStatus = Strings.FormatUpdatesApplyFailed(exception.Message);
            return;
        }

        this.AvailableUpdates.Clear();
        this.UpdatesDetail = string.Empty;
        this.UpdatesProblems = string.Empty;
        this.OnPropertyChanged(nameof(this.CanApplyUpdates));
        this.UpdatesStatus = Strings.FormatUpdatesApplied(count, profile);
        this.StatusMessage = this.UpdatesStatus;
        await this.LoadModsAsync(instance, profile).ConfigureAwait(true);
    }

    [RelayCommand]
    private void RequestSnapshotRestore()
    {
        string path = this.SnapshotRestorePath.Trim();
        if (!Path.IsPathFullyQualified(path))
        {
            this.StatusMessage = Strings.SnapshotPathNotAbsolute;
            return;
        }

        this.SnapshotRestoreText = Strings.FormatSnapshotConfirmRestore(path);
        this.IsConfirmingSnapshotRestore = true;
    }

    [RelayCommand]
    private void CancelSnapshotRestore() => this.IsConfirmingSnapshotRestore = false;

    [RelayCommand]
    private async Task ConfirmSnapshotRestoreAsync()
    {
        this.IsConfirmingSnapshotRestore = false;
        string path = this.SnapshotRestorePath.Trim();
        if (!this.CanRestoreSnapshot)
        {
            return;
        }

        try
        {
            long job = await this.client.StartSnapshotRestoreJobAsync(path, CancellationToken.None).ConfigureAwait(true);
            await this.FollowJobAsync(job).ConfigureAwait(true);
        }
        catch (Exception exception) when (exception is IOException or SocketException or JsonException or InvalidOperationException or KeyNotFoundException)
        {
            this.StatusMessage = Strings.FormatSnapshotRestoreFailed(exception.Message);
            return;
        }

        this.SnapshotRestorePath = string.Empty;
        await this.RefreshInstancesAsync().ConfigureAwait(true);
        this.StatusMessage = Strings.FormatSnapshotRestored(path);
    }

    /// <summary>Forgets what the History workspace found for the previous profile.</summary>
    private void ClearProfileHistory()
    {
        this.PendingRollback = null;
        this.Conflicts.Clear();
        this.ConflictsStatus = string.Empty;
        this.AvailableUpdates.Clear();
        this.UpdatesStatus = Strings.UpdatesNotChecked;
        this.UpdatesDetail = string.Empty;
        this.UpdatesProblems = string.Empty;
        this.OnPropertyChanged(nameof(this.CanApplyUpdates));
    }
}
