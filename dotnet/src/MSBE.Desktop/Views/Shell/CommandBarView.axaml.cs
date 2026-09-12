using System.Diagnostics.CodeAnalysis;

using Avalonia.Controls;

namespace MSBE.Desktop.Views.Shell;

/// <summary>Hosts shell menus and global actions.</summary>
[SuppressMessage("Design", "CA1515:Consider making public types internal", Justification = "Avalonia's external previewer must instantiate the view.")]
public partial class CommandBarView : UserControl
{
    /// <summary>Initializes a new instance of the <see cref="CommandBarView" /> class.</summary>
    public CommandBarView() => this.InitializeComponent();
}
