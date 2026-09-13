using System.Diagnostics.CodeAnalysis;

using Avalonia.Controls;

namespace MSBE.Desktop.Views.Shell;

/// <summary>Hosts the instance library.</summary>
[SuppressMessage("Design", "CA1515:Consider making public types internal", Justification = "Avalonia's external previewer must instantiate the view.")]
public partial class NavigationSidebarView : UserControl
{
    /// <summary>Initializes a new instance of the <see cref="NavigationSidebarView" /> class.</summary>
    public NavigationSidebarView() => this.InitializeComponent();
}
