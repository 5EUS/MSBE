namespace MSBE.Client;

/// <summary>A deployment still in effect on an instance.</summary>
/// <param name="Transaction">The deployment's transaction number.</param>
/// <param name="Profile">The profile it deployed.</param>
/// <param name="Files">How many files it placed.</param>
public sealed record JournalEntryInfo(long Transaction, string Profile, long Files);
