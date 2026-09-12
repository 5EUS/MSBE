using MSBE.Client;
using MSBE.Desktop.ViewModels;

using Xunit;

namespace MSBE.Desktop.Tests;

/// <summary>Tests for <see cref="MainViewModel" />.</summary>
public sealed class MainViewModelTests
{
    /// <summary>Refresh loads, sorts and selects registered instances.</summary>
    /// <returns>A task representing the test.</returns>
    [Fact]
    public async Task RefreshInstancesLoadsRegisteredInstances()
    {
        const string StatusJson = """{"name":"alpha","root":"/games/alpha","plan_id":"minecraft","plan_version":"1","loader":"fabric","game_version":"1.21.1","deployed_profile":"default","deployed_files":12}""";
        var client = new TestClient(arguments => arguments.Contains("list", StringComparer.Ordinal)
            ? new CommandResult(0, "[\"zeta\",\"alpha\"]", string.Empty)
            : new CommandResult(0, StatusJson, string.Empty));
        MainViewModel vm = new(client);

        await vm.RefreshInstancesCommand.ExecuteAsync(parameter: null);

        Assert.Equal(["alpha", "zeta"], vm.Instances);
        Assert.Equal("alpha", vm.SelectedInstance);
        Assert.Equal("/games/alpha", vm.SelectedInstanceRoot);
        Assert.Equal("minecraft 1", vm.SelectedInstancePlan);
        Assert.Equal("1.21.1 · fabric", vm.SelectedInstanceTarget);
        Assert.Equal("default · 12 managed files", vm.SelectedInstanceDeployment);
        Assert.Equal("2 registered instance(s).", vm.StatusMessage);

        vm.InstanceSearchText = "zet";

        Assert.Equal(["zeta"], vm.Instances);
    }

    /// <summary>Refresh exposes daemon failures as recoverable page state.</summary>
    /// <returns>A task representing the test.</returns>
    [Fact]
    public async Task RefreshInstancesReportsDaemonFailure()
    {
        MainViewModel vm = new(new TestClient(_ => new CommandResult(1, string.Empty, "instance store unavailable")));

        await vm.RefreshInstancesCommand.ExecuteAsync(parameter: null);

        Assert.True(vm.HasInstanceError);
        Assert.Equal("instance store unavailable", vm.InstanceError);
        Assert.Equal("Could not load instances.", vm.StatusMessage);
    }

    /// <summary>Native registration sends structured arguments and selects the new instance.</summary>
    /// <returns>A task representing the test.</returns>
    [Fact]
    public async Task AddInstanceRegistersAndSelectsInstance()
    {
        const string StatusJson = """{"name":"alpha","root":"/games/alpha","plan_id":"minecraft","plan_version":"1","loader":"fabric","game_version":"1.21.1","deployed_profile":null,"deployed_files":0}""";
        List<IReadOnlyList<string>> calls = [];
        var client = new TestClient(arguments =>
        {
            calls.Add(arguments);
            if (arguments.Contains("list", StringComparer.Ordinal))
            {
                return new CommandResult(0, "[\"alpha\"]", string.Empty);
            }

            return new CommandResult(0, StatusJson, string.Empty);
        });
        MainViewModel vm = new(client)
        {
            IsAddInstanceOpen = true,
            NewInstanceName = "alpha",
            NewInstanceRoot = "/games/alpha",
            NewInstancePlan = "/plans/minecraft.toml",
            NewInstanceLoader = "fabric",
            NewInstanceSide = "Client",
            NewInstanceGameVersion = "1.21.1",
        };

        await vm.SubmitAddInstanceCommand.ExecuteAsync(parameter: null);

        string[] expectedArguments =
        [
            "--format", "json", "instance", "add", "alpha",
            "--root", "/games/alpha", "--plan", "/plans/minecraft.toml",
            "--loader", "fabric", "--side", "client", "--game-version", "1.21.1",
        ];
        Assert.Contains(calls, arguments => arguments.SequenceEqual(expectedArguments, StringComparer.Ordinal));
        Assert.False(vm.IsAddInstanceOpen);
        Assert.Equal("alpha", vm.SelectedInstance);
        Assert.False(vm.HasAddInstanceError);
    }

    private sealed class TestClient : IMsbeClient
    {
        private readonly Func<IReadOnlyList<string>, CommandResult> runCommand;

        public TestClient(Func<IReadOnlyList<string>, CommandResult> runCommand) => this.runCommand = runCommand;

        public Task<DaemonInfo> GetInfoAsync(CancellationToken cancellationToken) => Task.FromResult(new DaemonInfo("test", 1));

        public Task<CommandResult> RunCommandAsync(IReadOnlyList<string> arguments, CancellationToken cancellationToken) => Task.FromResult(this.runCommand(arguments));
    }
}
