namespace MSBE.Desktop.ViewModels;

/// <summary>A pack-owned config file and its exact content digest.</summary>
/// <param name="Path">The game-relative destination path.</param>
/// <param name="Digest">The content-addressed digest.</param>
internal sealed record PackConfigItem(string Path, string Digest);
