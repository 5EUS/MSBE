using System.Diagnostics.CodeAnalysis;

using MSBE.Client;
using MSBE.Desktop.Resources;

namespace MSBE.Desktop.ViewModels;

/// <summary>A mod with a newer compatible release, as the updates view shows it.</summary>
[SuppressMessage("Design", "CA1515:Consider making public types internal", Justification = "Compiled AXAML item templates reference this type directly.")]
public sealed class UpdateItem
{
    /// <summary>Initializes a new instance of the <see cref="UpdateItem" /> class.</summary>
    /// <param name="update">The update, as the daemon reported it.</param>
    internal UpdateItem(ModUpdateInfo update)
    {
        this.Module = update.Module;
        this.Change = Strings.FormatUpdateChange(update.From, update.To);
    }

    /// <summary>Gets the mod.</summary>
    public string Module { get; }

    /// <summary>Gets the release in the profile and the one it updates to.</summary>
    public string Change { get; }
}
