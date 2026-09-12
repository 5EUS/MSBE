namespace MSBE.Desktop.ViewModels;

/// <summary>A file a capture would adopt into the profile.</summary>
/// <param name="Path">The game-relative path.</param>
/// <param name="Kind">Whether it changed or is new.</param>
/// <param name="Diff">A line diff for small text files, or an empty string.</param>
internal sealed record PackCaptureItem(string Path, string Kind, string Diff);
