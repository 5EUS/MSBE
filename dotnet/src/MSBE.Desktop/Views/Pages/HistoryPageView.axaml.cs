using System.Diagnostics.CodeAnalysis;

using Avalonia.Controls;

namespace MSBE.Desktop.Views.Pages;

/// <summary>Shows the selected instance's deployments, conflicts, updates and snapshots.</summary>
[SuppressMessage("Design", "CA1515:Consider making public types internal", Justification = "Avalonia's external previewer must instantiate the view.")]
public partial class HistoryPageView : UserControl
{
    /// <summary>Initializes a new instance of the <see cref="HistoryPageView" /> class.</summary>
    public HistoryPageView() => this.InitializeComponent();
}
