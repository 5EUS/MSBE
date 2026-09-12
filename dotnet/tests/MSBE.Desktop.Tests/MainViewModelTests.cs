using MSBE.Client;
using MSBE.Desktop.ViewModels;

using Xunit;

namespace MSBE.Desktop.Tests;

/// <summary>Tests for <see cref="MainViewModel" />.</summary>
public sealed class MainViewModelTests
{
    /// <summary>The status message reports the instance count after detection.</summary>
    /// <returns>A task representing the test.</returns>
    [Fact]
    public async Task DetectInstancesReportsCount()
    {
        MainViewModel vm = new(new TestClient());

        await vm.DetectInstancesCommand.ExecuteAsync(parameter: null);

        Assert.Equal("0 instance(s) detected.", vm.StatusMessage);
    }

    private sealed class TestClient : IMsbeClient
    {
        public Task<DaemonInfo> GetInfoAsync(CancellationToken cancellationToken) => Task.FromResult(new DaemonInfo("test", 1));

        public Task<CommandResult> RunCommandAsync(IReadOnlyList<string> arguments, CancellationToken cancellationToken) => Task.FromResult(new CommandResult(0, string.Empty, string.Empty));
    }
}
