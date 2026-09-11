using CommunityToolkit.Mvvm.ComponentModel;

namespace MSBE.Desktop.ViewModels;

/// <summary>Base class for view models.</summary>
/// <remarks>
/// <see cref="ObservableObject" /> is source-generated, which is what makes it usable
/// under NativeAOT; reflection-based MVVM frameworks are not.
/// </remarks>
internal abstract class ViewModelBase : ObservableObject;
