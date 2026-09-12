using System.Diagnostics.CodeAnalysis;

using Avalonia.Controls;

namespace MSBE.Desktop.Views.Pages;

/// <summary>Edits pack-owned configuration and exports reproducible packs.</summary>
[SuppressMessage("Design", "CA1515:Consider making public types internal", Justification = "Avalonia's external previewer must instantiate the view.")]
public partial class PackPageView : UserControl
{
    /// <summary>Initializes a new instance of the <see cref="PackPageView" /> class.</summary>
    public PackPageView() => this.InitializeComponent();
}
