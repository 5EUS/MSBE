namespace MSBE.Client;

/// <summary>What a rollback undid, and the deployments left in effect.</summary>
/// <param name="RolledBack">The transactions undone, newest first.</param>
/// <param name="Journal">The deployments still in effect, oldest first.</param>
public sealed record RollbackInfo(IReadOnlyList<long> RolledBack, IReadOnlyList<JournalEntryInfo> Journal);
