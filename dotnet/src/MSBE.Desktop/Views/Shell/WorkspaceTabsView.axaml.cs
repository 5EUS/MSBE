using System.Diagnostics.CodeAnalysis;

using Avalonia.Controls;

namespace MSBE.Desktop.Views.Shell;

/// <summary>Hosts the top-level workspace tabs beneath the command bar.</summary>
[SuppressMessage("Design", "CA1515:Consider making public types internal", Justification = "Avalonia's external previewer must instantiate the view.")]
public partial class WorkspaceTabsView : UserControl
{
    /// <summary>Initializes a new instance of the <see cref="WorkspaceTabsView" /> class.</summary>
    public WorkspaceTabsView() => this.InitializeComponent();
}
