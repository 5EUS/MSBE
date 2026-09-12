namespace MSBE.Desktop.ViewModels;

/// <summary>One file, requirement or change in a pack preview.</summary>
/// <param name="Group">How the preview classifies it.</param>
/// <param name="Subject">The path, module or change.</param>
internal sealed record PackPreviewItem(string Group, string Subject);
