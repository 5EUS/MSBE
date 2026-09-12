namespace MSBE.Client;

/// <summary>An error the daemon returned for an RPC request.</summary>
/// <remarks>
/// Derives from <see cref="InvalidOperationException" /> so callers that already treat daemon
/// failures as recoverable state keep doing so.
/// </remarks>
public sealed class MsbeRpcException : InvalidOperationException
{
    /// <summary>Initializes a new instance of the <see cref="MsbeRpcException" /> class.</summary>
    public MsbeRpcException()
    {
    }

    /// <summary>Initializes a new instance of the <see cref="MsbeRpcException" /> class.</summary>
    /// <param name="message">The daemon's message.</param>
    public MsbeRpcException(string message)
        : base(message)
    {
    }

    /// <summary>Initializes a new instance of the <see cref="MsbeRpcException" /> class.</summary>
    /// <param name="message">The daemon's message.</param>
    /// <param name="innerException">The underlying failure.</param>
    public MsbeRpcException(string message, Exception innerException)
        : base(message, innerException)
    {
    }

    /// <summary>Initializes a new instance of the <see cref="MsbeRpcException" /> class.</summary>
    /// <param name="message">The daemon's message.</param>
    /// <param name="code">The JSON-RPC error code.</param>
    /// <param name="failureCode">The stable pack failure code, when the daemon reported one.</param>
    public MsbeRpcException(string message, int code, string? failureCode)
        : base(message)
    {
        this.Code = code;
        this.FailureCode = failureCode;
    }

    /// <summary>Gets the JSON-RPC error code.</summary>
    public int Code { get; }

    /// <summary>Gets the stable pack failure code, such as <c>StalePlan</c>, when there is one.</summary>
    public string? FailureCode { get; }
}
