using System.Diagnostics.CodeAnalysis;

using Avalonia.Controls;

namespace MSBE.Desktop.Views.Pages;

/// <summary>Shows the running download, what is up next, and what has finished.</summary>
[SuppressMessage("Design", "CA1515:Consider making public types internal", Justification = "Avalonia's external previewer must instantiate the view.")]
public partial class DownloadsPageView : UserControl
{
    /// <summary>Initializes a new instance of the <see cref="DownloadsPageView" /> class.</summary>
    public DownloadsPageView() => this.InitializeComponent();
}
