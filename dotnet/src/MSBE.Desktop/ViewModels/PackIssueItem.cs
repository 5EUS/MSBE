namespace MSBE.Desktop.ViewModels;

/// <summary>A blocker or warning a pack preview reported.</summary>
/// <param name="IsBlocker">Whether the issue prevents execution.</param>
/// <param name="Code">The stable issue code.</param>
/// <param name="Message">Human-readable detail.</param>
internal sealed record PackIssueItem(bool IsBlocker, string Code, string Message)
{
    /// <summary>Gets the severity label.</summary>
    public string Severity => this.IsBlocker ? "Blocked" : "Warning";
}
