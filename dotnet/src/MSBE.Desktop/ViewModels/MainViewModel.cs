using CommunityToolkit.Mvvm.ComponentModel;

namespace MSBE.Desktop.ViewModels;

/// <summary>
/// The shell view model.
/// </summary>
/// <remarks>
/// Split across <c>MainViewModel.*.cs</c> partials, one per feature area, so that the
/// shell stays navigable as it grows. Members belong in the partial named for the
/// surface they serve, never here — this file holds only shell-wide state.
/// </remarks>
internal sealed partial class MainViewModel : ViewModelBase
{
    /// <summary>Gets or sets the window title.</summary>
    [ObservableProperty]
    public partial string Title { get; set; } = "MSBE";

    /// <summary>Gets or sets the message shown in the status bar.</summary>
    [ObservableProperty]
    public partial string StatusMessage { get; set; } = "Not connected to a daemon.";
}
