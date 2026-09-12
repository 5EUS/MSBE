namespace MSBE.Client;

/// <summary>A daemon job's state and the events a client has not seen.</summary>
/// <param name="JobId">The job.</param>
/// <param name="State"><c>queued</c>, <c>running</c>, <c>succeeded</c>, <c>failed</c> or <c>cancelled</c>.</param>
/// <param name="Events">Events after the requested sequence.</param>
/// <param name="Next">The sequence to request after next time.</param>
public sealed record JobStatusInfo(long JobId, string State, IReadOnlyList<JobEventInfo> Events, long Next)
{
    /// <summary>Gets a value indicating whether the job has finished.</summary>
    public bool IsFinished => this.State is "succeeded" or "failed" or "cancelled";
}
