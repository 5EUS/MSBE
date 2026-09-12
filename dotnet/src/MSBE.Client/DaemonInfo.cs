namespace MSBE.Client;

/// <summary>Identifies a daemon and the RPC contract version it serves.</summary>
/// <param name="Version">The daemon's own version string.</param>
/// <param name="RpcVersion">The highest RPC contract version the daemon speaks.</param>
/// <param name="DataDirectory">The data directory holding MSBE's instances and state, or <see langword="null" /> if the daemon did not report one.</param>
public sealed record DaemonInfo(string Version, int RpcVersion, string? DataDirectory);
