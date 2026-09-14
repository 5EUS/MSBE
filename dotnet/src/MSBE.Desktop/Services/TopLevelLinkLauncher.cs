using Avalonia.Controls;

namespace MSBE.Desktop.Services;

/// <summary>Opens web pages through the launcher of the application's top-level window.</summary>
internal sealed class TopLevelLinkLauncher : ILinkLauncher
{
    private readonly Func<TopLevel?> topLevel;

    /// <summary>Initializes a new instance of the <see cref="TopLevelLinkLauncher" /> class.</summary>
    /// <param name="topLevel">Returns the window whose launcher opens pages, once it exists.</param>
    public TopLevelLinkLauncher(Func<TopLevel?> topLevel) => this.topLevel = topLevel;

    /// <inheritdoc />
    public Task<bool> OpenAsync(Uri page) => this.topLevel() is { } window
        ? window.Launcher.LaunchUriAsync(page)
        : Task.FromResult(false);
}
