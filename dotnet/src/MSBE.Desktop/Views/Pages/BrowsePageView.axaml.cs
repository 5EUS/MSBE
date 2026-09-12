using System.Diagnostics.CodeAnalysis;

using Avalonia.Controls;

namespace MSBE.Desktop.Views.Pages;

/// <summary>Searches compatible providers and adds results to a profile.</summary>
[SuppressMessage("Design", "CA1515:Consider making public types internal", Justification = "Avalonia's external previewer must instantiate the view.")]
public partial class BrowsePageView : UserControl
{
    /// <summary>Initializes a new instance of the <see cref="BrowsePageView" /> class.</summary>
    public BrowsePageView() => this.InitializeComponent();
}
