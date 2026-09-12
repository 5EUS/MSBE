using System.Diagnostics.CodeAnalysis;

namespace MSBE.Desktop.ViewModels;

/// <summary>One source file deliberately withheld from deployment.</summary>
/// <param name="Module">The owning mod.</param>
/// <param name="Source">The source path.</param>
/// <param name="Reason">The exclusion rule summary.</param>
[SuppressMessage("Design", "CA1515:Consider making public types internal", Justification = "Compiled AXAML item templates reference this type directly.")]
public sealed record DeploymentExclusionItem(string Module, string Source, string Reason);
