using System.Diagnostics.CodeAnalysis;

using Avalonia.Controls;

using MSBE.Desktop.ViewModels;

namespace MSBE.Desktop.Views.Shell;

/// <summary>The application shell window.</summary>
[SuppressMessage("Design", "CA1515:Consider making public types internal", Justification = "Avalonia's external previewer must instantiate the view.")]
public partial class MainWindow : Window
{
    private CliWindow? cliWindow;

    /// <summary>Initializes a new instance of the <see cref="MainWindow" /> class.</summary>
    public MainWindow()
    {
        this.InitializeComponent();
        this.DataContextChanged += this.OnDataContextChanged;
    }

    private void OnDataContextChanged(object? sender, EventArgs eventArgs)
    {
        if (this.DataContext is MainViewModel viewModel)
        {
            viewModel.PropertyChanged += this.OnViewModelPropertyChanged;
        }
    }

    private void OnViewModelPropertyChanged(object? sender, System.ComponentModel.PropertyChangedEventArgs eventArgs)
    {
        if (string.Equals(eventArgs.PropertyName, nameof(MainViewModel.IsCliOpen), StringComparison.Ordinal) && sender is MainViewModel { IsCliOpen: true } viewModel)
        {
            this.cliWindow ??= new CliWindow();
            this.cliWindow.DataContext = viewModel;
            this.cliWindow.Show(this);
        }
    }
}
