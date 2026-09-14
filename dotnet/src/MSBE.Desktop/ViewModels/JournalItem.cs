using System.Diagnostics.CodeAnalysis;

using MSBE.Client;
using MSBE.Desktop.Resources;

namespace MSBE.Desktop.ViewModels;

/// <summary>A deployment still in effect, as the History workspace shows it.</summary>
[SuppressMessage("Design", "CA1515:Consider making public types internal", Justification = "Compiled AXAML item templates reference this type directly.")]
public sealed class JournalItem
{
    /// <summary>Initializes a new instance of the <see cref="JournalItem" /> class.</summary>
    /// <param name="entry">The deployment, as the daemon reported it.</param>
    /// <param name="isDeployed">Whether it is the deployment in effect now.</param>
    internal JournalItem(JournalEntryInfo entry, bool isDeployed)
    {
        this.Transaction = entry.Transaction;
        this.Title = Strings.FormatJournalTransaction(entry.Transaction);
        this.Detail = Strings.FormatJournalDetail(entry.Profile, entry.Files);
        this.RollBackName = Strings.FormatJournalRollBackToName(entry.Transaction);
        this.IsDeployed = isDeployed;
    }

    /// <summary>Gets the deployment's transaction number.</summary>
    public long Transaction { get; }

    /// <summary>Gets the deployment's heading.</summary>
    public string Title { get; }

    /// <summary>Gets the profile it deployed and how many files it placed.</summary>
    public string Detail { get; }

    /// <summary>Gets the name screen readers give its rollback action.</summary>
    public string RollBackName { get; }

    /// <summary>Gets a value indicating whether it is the deployment in effect now.</summary>
    public bool IsDeployed { get; }
}
