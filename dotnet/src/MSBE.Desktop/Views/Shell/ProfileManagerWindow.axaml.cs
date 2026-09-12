using System.Diagnostics.CodeAnalysis;

using Avalonia.Controls;

using MSBE.Desktop.ViewModels;

namespace MSBE.Desktop.Views.Shell;

/// <summary>Creates, clones, and removes profiles for the selected instance.</summary>
[SuppressMessage("Design", "CA1515:Consider making public types internal", Justification = "Avalonia's external previewer must instantiate the view.")]
public partial class ProfileManagerWindow : Window
{
    /// <summary>Initializes a new instance of the <see cref="ProfileManagerWindow" /> class.</summary>
    public ProfileManagerWindow()
    {
        this.InitializeComponent();
        this.Closed += this.OnClosed;
    }

    private void OnClosed(object? sender, EventArgs eventArgs)
    {
        if (this.DataContext is MainViewModel viewModel)
        {
            viewModel.IsProfileManagerOpen = false;
        }
    }
}
