using System.Diagnostics.CodeAnalysis;

using Avalonia.Controls;

namespace MSBE.Desktop.Views.Pages;

/// <summary>Lists games and ecosystems supported by the connected daemon.</summary>
[SuppressMessage("Design", "CA1515:Consider making public types internal", Justification = "Avalonia's external previewer must instantiate the view.")]
public partial class GamesPageView : UserControl
{
    /// <summary>Initializes a new instance of the <see cref="GamesPageView" /> class.</summary>
    public GamesPageView() => this.InitializeComponent();
}
