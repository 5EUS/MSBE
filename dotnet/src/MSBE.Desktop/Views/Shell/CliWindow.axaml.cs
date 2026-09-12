using System.Diagnostics.CodeAnalysis;

using Avalonia.Controls;

using MSBE.Desktop.ViewModels;

namespace MSBE.Desktop.Views.Shell;

/// <summary>Hosts an independent daemon command-line window.</summary>
[SuppressMessage("Design", "CA1515:Consider making public types internal", Justification = "Avalonia's external previewer must instantiate the view.")]
public partial class CliWindow : Window
{
    /// <summary>Initializes a new instance of the <see cref="CliWindow" /> class.</summary>
    public CliWindow()
    {
        this.InitializeComponent();
        this.Closed += this.OnClosed;
    }

    private void OnClosed(object? sender, EventArgs eventArgs)
    {
        if (this.DataContext is MainViewModel viewModel)
        {
            viewModel.IsCliOpen = false;
        }
    }
}
