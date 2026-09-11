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
        MainViewModel vm = new();

        await vm.DetectInstancesCommand.ExecuteAsync(parameter: null);

        Assert.Equal("0 instance(s) detected.", vm.StatusMessage);
    }
}
