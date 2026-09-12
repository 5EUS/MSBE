using System.Diagnostics.CodeAnalysis;

using Avalonia.Controls;

namespace MSBE.Desktop.Views.Pages;

/// <summary>Lists detected game instances and offers re-detection.</summary>
[SuppressMessage("Design", "CA1515:Consider making public types internal", Justification = "Avalonia's external previewer must instantiate the view.")]
public partial class InstancesPageView : UserControl
{
    /// <summary>Initializes a new instance of the <see cref="InstancesPageView" /> class.</summary>
    public InstancesPageView() => this.InitializeComponent();
}
