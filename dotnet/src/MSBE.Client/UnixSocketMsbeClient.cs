using System.Buffers;
using System.Diagnostics;
using System.Net.Sockets;
using System.Text;
using System.Text.Json;

namespace MSBE.Client;

/// <summary>Connects to the local MSBE daemon over its Unix domain socket.</summary>
public sealed class UnixSocketMsbeClient : IMsbeClient
{
    private static readonly SemaphoreSlim DaemonStartupLock = new(1, 1);
    private readonly string socketPath;
    private long nextRequestId;

    /// <summary>Initializes a new instance of the <see cref="UnixSocketMsbeClient" /> class for the default daemon socket.</summary>
    public UnixSocketMsbeClient()
        : this(GetDefaultSocketPath())
    {
    }

    /// <summary>Initializes a new instance of the <see cref="UnixSocketMsbeClient" /> class for <paramref name="socketPath" />.</summary>
    /// <param name="socketPath">The daemon's Unix domain socket path.</param>
    public UnixSocketMsbeClient(string socketPath) => this.socketPath = socketPath;

    /// <inheritdoc />
    public async Task<DaemonInfo> GetInfoAsync(CancellationToken cancellationToken)
    {
        using JsonDocument response = await this.SendAsync("daemon.info", null, cancellationToken).ConfigureAwait(false);
        JsonElement result = GetResult(response.RootElement);
        return new DaemonInfo(
            result.GetProperty("version").GetString() ?? string.Empty,
            result.GetProperty("rpc_version").GetInt32());
    }

    /// <inheritdoc />
    public async Task<IReadOnlyList<GameInfo>> GetGamesAsync(CancellationToken cancellationToken)
    {
        using JsonDocument response = await this.SendAsync("game.list", null, cancellationToken).ConfigureAwait(false);
        JsonElement result = GetResult(response.RootElement);
        var games = new List<GameInfo>();
        foreach (JsonElement game in result.EnumerateArray())
        {
            games.Add(new GameInfo(
                game.GetProperty("id").GetString() ?? string.Empty,
                game.GetProperty("name").GetString() ?? string.Empty,
                game.GetProperty("support_version").GetString() ?? string.Empty,
                game.GetProperty("loaders").EnumerateArray().Select(loader => loader.GetString() ?? string.Empty).ToArray()));
        }

        return games;
    }

    /// <inheritdoc />
    public async Task<CommandResult> RunCommandAsync(IReadOnlyList<string> arguments, CancellationToken cancellationToken)
    {
        ArgumentNullException.ThrowIfNull(arguments);
        using JsonDocument response = await this.SendAsync("command.run", arguments, cancellationToken).ConfigureAwait(false);
        JsonElement result = GetResult(response.RootElement);
        return new CommandResult(
            result.GetProperty("exit_code").GetInt32(),
            result.GetProperty("stdout").GetString() ?? string.Empty,
            result.GetProperty("stderr").GetString() ?? string.Empty);
    }

    private static string GetDefaultSocketPath()
    {
        string runtimeDirectory = Environment.GetEnvironmentVariable("XDG_RUNTIME_DIR") ?? Path.GetTempPath();
        return Path.Combine(runtimeDirectory, "msbe.sock");
    }

    private static void StartDaemon(string socketPath)
    {
        string executableName = OperatingSystem.IsWindows() ? "msbe-daemon.exe" : "msbe-daemon";
        string executablePath = Path.Combine(AppContext.BaseDirectory, executableName);
        var startInfo = new ProcessStartInfo
        {
            FileName = executablePath,
            WorkingDirectory = AppContext.BaseDirectory,
            UseShellExecute = false,
            CreateNoWindow = true,
        };
        startInfo.ArgumentList.Add("--socket");
        startInfo.ArgumentList.Add(socketPath);

        try
        {
            using Process process = Process.Start(startInfo)
                ?? throw new IOException($"Cannot start {executablePath}.");
        }
        catch (System.ComponentModel.Win32Exception exception)
        {
            throw new IOException($"Cannot start {executablePath}.", exception);
        }
    }

    private static JsonElement GetResult(JsonElement response)
    {
        if (response.TryGetProperty("result", out JsonElement result))
        {
            return result;
        }

        string message = response.TryGetProperty("error", out JsonElement error) && error.TryGetProperty("message", out JsonElement errorMessage)
            ? errorMessage.GetString() ?? "The daemon returned an unknown error."
            : "The daemon returned an invalid JSON-RPC response.";
        throw new InvalidOperationException(message);
    }

    private static byte[] CreateRequest(long requestId, string method, IReadOnlyList<string>? arguments)
    {
        var buffer = new ArrayBufferWriter<byte>();
#pragma warning disable MA0045 // Utf8JsonWriter does not implement IAsyncDisposable.
        using (var writer = new Utf8JsonWriter(buffer))
        {
            writer.WriteStartObject();
            writer.WriteString("jsonrpc", "2.0");
            writer.WriteNumber("id", requestId);
            writer.WriteString("method", method);
            writer.WritePropertyName("params");
            if (arguments is null)
            {
                writer.WriteNullValue();
            }
            else
            {
                writer.WriteStartObject();
                writer.WriteStartArray("args");
                foreach (string argument in arguments)
                {
                    writer.WriteStringValue(argument);
                }

                writer.WriteEndArray();
                writer.WriteEndObject();
            }

            writer.WriteEndObject();
        }
#pragma warning restore MA0045

        byte[] request = new byte[buffer.WrittenCount + 1];
        buffer.WrittenSpan.CopyTo(request);
        request[^1] = (byte)'\n';
        return request;
    }

    private async Task<JsonDocument> SendAsync(string method, IReadOnlyList<string>? arguments, CancellationToken cancellationToken)
    {
        using Socket socket = await this.ConnectAsync(cancellationToken).ConfigureAwait(false);
        var stream = new NetworkStream(socket, ownsSocket: false);
        await using (stream.ConfigureAwait(false))
        {
            long requestId = Interlocked.Increment(ref this.nextRequestId);
            byte[] request = CreateRequest(requestId, method, arguments);
            await stream.WriteAsync(request, cancellationToken).ConfigureAwait(false);

#pragma warning disable MA0045 // StreamReader does not implement IAsyncDisposable.
            using var reader = new StreamReader(stream, Encoding.UTF8, leaveOpen: true);
#pragma warning restore MA0045
            string response = await reader.ReadLineAsync(cancellationToken).ConfigureAwait(false)
                ?? throw new IOException("The daemon closed the connection before replying.");
            return JsonDocument.Parse(response);
        }
    }

    private async Task<Socket> ConnectAsync(CancellationToken cancellationToken)
    {
        try
        {
            return await this.OpenSocketAsync(cancellationToken).ConfigureAwait(false);
        }
        catch (SocketException)
        {
            await this.EnsureDaemonStartedAsync(cancellationToken).ConfigureAwait(false);
        }

        for (int attempt = 0; attempt < 100; attempt++)
        {
            try
            {
                return await this.OpenSocketAsync(cancellationToken).ConfigureAwait(false);
            }
            catch (SocketException) when (attempt < 99)
            {
                await Task.Delay(TimeSpan.FromMilliseconds(20), cancellationToken).ConfigureAwait(false);
            }
        }

        throw new IOException($"Cannot connect to msbe-daemon at {this.socketPath}.");
    }

    private async Task EnsureDaemonStartedAsync(CancellationToken cancellationToken)
    {
        await DaemonStartupLock.WaitAsync(cancellationToken).ConfigureAwait(false);
        try
        {
            try
            {
                using Socket socket = await this.OpenSocketAsync(cancellationToken).ConfigureAwait(false);
                return;
            }
            catch (SocketException)
            {
                StartDaemon(this.socketPath);
            }
        }
        finally
        {
            DaemonStartupLock.Release();
        }
    }

    private async Task<Socket> OpenSocketAsync(CancellationToken cancellationToken)
    {
        Socket socket = new(AddressFamily.Unix, SocketType.Stream, ProtocolType.Unspecified);
        try
        {
            await socket.ConnectAsync(new UnixDomainSocketEndPoint(this.socketPath), cancellationToken).ConfigureAwait(false);
            return socket;
        }
        catch
        {
            socket.Dispose();
            throw;
        }
    }
}
