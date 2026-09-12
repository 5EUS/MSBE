namespace MSBE.Client;

/// <summary>Output returned by a daemon-owned command execution.</summary>
/// <param name="ExitCode">The command exit code.</param>
/// <param name="StandardOutput">The command standard output.</param>
/// <param name="StandardError">The command diagnostic output.</param>
public sealed record CommandResult(int ExitCode, string StandardOutput, string StandardError);
