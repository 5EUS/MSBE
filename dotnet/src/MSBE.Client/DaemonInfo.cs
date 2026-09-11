namespace MSBE.Client;

/// <summary>Identifies a daemon and the RPC contract version it serves.</summary>
/// <param name="Version">The daemon's own version string.</param>
/// <param name="RpcVersion">The highest RPC contract version the daemon speaks.</param>
public sealed record DaemonInfo(string Version, int RpcVersion);
