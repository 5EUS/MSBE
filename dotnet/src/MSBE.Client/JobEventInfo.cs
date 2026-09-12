namespace MSBE.Client;

/// <summary>Something a daemon job reported.</summary>
/// <param name="Sequence">The event's position in its job's log.</param>
/// <param name="Kind"><c>progress</c>, <c>done</c>, <c>failed</c> or <c>cancelled</c>.</param>
/// <param name="Message">Progress or failure detail.</param>
/// <param name="Completed">Steps done, for progress.</param>
/// <param name="Total">Steps in total, for progress.</param>
/// <param name="Code">The stable failure code, for a failure.</param>
public sealed record JobEventInfo(long Sequence, string Kind, string Message, long Completed, long Total, string? Code);
