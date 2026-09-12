using System.Diagnostics.CodeAnalysis;

using Avalonia.Controls;

using MSBE.Desktop.ViewModels;

namespace MSBE.Desktop.Views.Shell;

/// <summary>Shows the exact filesystem changes before deployment.</summary>
[SuppressMessage("Design", "CA1515:Consider making public types internal", Justification = "Avalonia's external previewer must instantiate the view.")]
public partial class DeploymentPreviewWindow : Window
{
    /// <summary>Initializes a new instance of the <see cref="DeploymentPreviewWindow" /> class.</summary>
    public DeploymentPreviewWindow()
    {
        this.InitializeComponent();
        this.Closed += this.OnClosed;
    }

    private void OnClosed(object? sender, EventArgs eventArgs)
    {
        if (this.DataContext is MainViewModel viewModel)
        {
            viewModel.IsDeploymentPreviewOpen = false;
        }
    }
}
