using Avalonia.Controls;
using Avalonia.Platform.Storage;

namespace MSBE.Desktop.Services;

/// <summary>Opens folders through the launcher of the application's top-level window.</summary>
internal sealed class TopLevelFolderLauncher : IFolderLauncher
{
    private readonly Func<TopLevel?> topLevel;

    /// <summary>Initializes a new instance of the <see cref="TopLevelFolderLauncher" /> class.</summary>
    /// <param name="topLevel">Returns the window whose launcher opens folders, once it exists.</param>
    public TopLevelFolderLauncher(Func<TopLevel?> topLevel) => this.topLevel = topLevel;

    /// <inheritdoc />
    public Task<bool> OpenAsync(string path) => this.topLevel() is { } window
        ? window.Launcher.LaunchDirectoryInfoAsync(new DirectoryInfo(path))
        : Task.FromResult(false);
}
