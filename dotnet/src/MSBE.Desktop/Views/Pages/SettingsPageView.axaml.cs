using System.Diagnostics.CodeAnalysis;

using Avalonia.Controls;

namespace MSBE.Desktop.Views.Pages;

/// <summary>Shows settings and shortcuts into MSBE's own state.</summary>
[SuppressMessage("Design", "CA1515:Consider making public types internal", Justification = "Avalonia's external previewer must instantiate the view.")]
public partial class SettingsPageView : UserControl
{
    /// <summary>Initializes a new instance of the <see cref="SettingsPageView" /> class.</summary>
    public SettingsPageView() => this.InitializeComponent();
}
