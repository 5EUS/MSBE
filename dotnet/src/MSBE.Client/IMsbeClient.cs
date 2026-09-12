namespace MSBE.Client;

/// <summary>A connection to the MSBE daemon.</summary>
/// <remarks>
/// The UI never holds a credential and never touches the filesystem; it asks the daemon,
/// which is the single writer. See <c>docs/03-architecture.md</c>.
/// </remarks>
public interface IMsbeClient
{
    /// <summary>Retrieves the daemon's identity and negotiated contract version.</summary>
    /// <param name="cancellationToken">Cancels the pending request.</param>
    /// <returns>The daemon's reported information.</returns>
    Task<DaemonInfo> GetInfoAsync(CancellationToken cancellationToken);

    /// <summary>Lists games currently supported by the daemon.</summary>
    /// <param name="cancellationToken">Cancels the pending request.</param>
    /// <returns>The loaded games and their supported ecosystems.</returns>
    Task<IReadOnlyList<GameInfo>> GetGamesAsync(CancellationToken cancellationToken);

    /// <summary>Runs one MSBE command through the daemon.</summary>
    /// <param name="arguments">The command arguments, excluding the executable name.</param>
    /// <param name="cancellationToken">Cancels the pending request.</param>
    /// <returns>The command result produced by the daemon.</returns>
    Task<CommandResult> RunCommandAsync(IReadOnlyList<string> arguments, CancellationToken cancellationToken);
}
