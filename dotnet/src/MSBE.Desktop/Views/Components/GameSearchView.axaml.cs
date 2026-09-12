using System.Diagnostics.CodeAnalysis;

using Avalonia.Controls;

namespace MSBE.Desktop.Views.Components;

/// <summary>Searches daemon-supported games and selects one for a workflow.</summary>
[SuppressMessage("Design", "CA1515:Consider making public types internal", Justification = "Avalonia's external previewer must instantiate the view.")]
public partial class GameSearchView : UserControl
{
    /// <summary>Initializes a new instance of the <see cref="GameSearchView" /> class.</summary>
    public GameSearchView() => this.InitializeComponent();
}
