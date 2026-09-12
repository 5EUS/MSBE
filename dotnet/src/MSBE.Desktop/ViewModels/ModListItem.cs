using System.Diagnostics.CodeAnalysis;

namespace MSBE.Desktop.ViewModels;

/// <summary>A mod displayed in an instance profile.</summary>
/// <param name="Name">The stable local mod name.</param>
/// <param name="Origin">The original artifact name.</param>
/// <param name="Source">The provider or local-source label.</param>
/// <param name="Version">The provider version, when known.</param>
/// <param name="FileCount">The number of stored files in the artifact.</param>
[SuppressMessage("Design", "CA1515:Consider making public types internal", Justification = "Compiled AXAML item templates reference this type directly.")]
public sealed record ModListItem(string Name, string Origin, string Source, string Version, int FileCount)
{
    /// <summary>Gets a compact file-count label.</summary>
    public string FileSummary => this.FileCount == 1 ? "1 file" : $"{this.FileCount} files";

    /// <summary>Gets a one-character visual identifier.</summary>
    public string Monogram => this.Name.Length == 0 ? "?" : this.Name[..1].ToUpperInvariant();
}
