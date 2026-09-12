using System.Text.Json;

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
    /// <remarks>A compatibility bridge for surfaces that have no typed RPC method yet.</remarks>
    Task<CommandResult> RunCommandAsync(IReadOnlyList<string> arguments, CancellationToken cancellationToken);

    /// <summary>Invokes one typed RPC method.</summary>
    /// <param name="method">The JSON-RPC method name.</param>
    /// <param name="writeParameters">Writes the parameters value, or <see langword="null" /> for none.</param>
    /// <param name="cancellationToken">Cancels the pending request.</param>
    /// <returns>The method result, detached from the response so it outlives it.</returns>
    /// <exception cref="MsbeRpcException">The daemon answered with an error.</exception>
    Task<JsonElement> InvokeAsync(string method, Action<Utf8JsonWriter>? writeParameters, CancellationToken cancellationToken);
}
