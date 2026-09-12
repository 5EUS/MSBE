using System.Diagnostics.CodeAnalysis;

namespace MSBE.Desktop.ViewModels;

/// <summary>One filesystem change shown before deployment.</summary>
/// <param name="Action">The concise operation label.</param>
/// <param name="Path">The destination path.</param>
/// <param name="Marker">The visual operation marker.</param>
[SuppressMessage("Design", "CA1515:Consider making public types internal", Justification = "Compiled AXAML item templates reference this type directly.")]
public sealed record DeploymentPreviewItem(string Action, string Path, string Marker);
