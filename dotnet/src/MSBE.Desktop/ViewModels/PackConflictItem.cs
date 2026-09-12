using CommunityToolkit.Mvvm.ComponentModel;

namespace MSBE.Desktop.ViewModels;

/// <summary>A profile change the updated pack no longer applies cleanly to, awaiting a resolution.</summary>
internal sealed partial class PackConflictItem : ObservableObject
{
    /// <summary>Initializes a new instance of the <see cref="PackConflictItem" /> class.</summary>
    /// <param name="id">The conflict ID a resolution names.</param>
    /// <param name="reason">Why the change no longer applies.</param>
    /// <param name="resolution">The resolution already supplied, if any.</param>
    public PackConflictItem(string id, string reason, string? resolution)
    {
        this.Id = id;
        this.Reason = reason;
        this.Resolution = resolution ?? string.Empty;
    }

    /// <summary>Gets the conflict ID.</summary>
    public string Id { get; }

    /// <summary>Gets why the change no longer applies.</summary>
    public string Reason { get; }

    /// <summary>Gets the resolutions a user can choose.</summary>
    public IReadOnlyList<string> Choices { get; } = ["keep", "drop"];

    /// <summary>Gets or sets the chosen resolution, or an empty string while unresolved.</summary>
    [ObservableProperty]
    public partial string Resolution { get; set; }
}
