using System.Diagnostics.CodeAnalysis;

using Avalonia.Controls;

using MSBE.Desktop.ViewModels;

namespace MSBE.Desktop.Views.Shell;

/// <summary>The application shell window.</summary>
[SuppressMessage("Design", "CA1515:Consider making public types internal", Justification = "Avalonia's external previewer must instantiate the view.")]
public partial class MainWindow : Window
{
    private AddInstanceWindow? addInstanceWindow;
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
        if (sender is not MainViewModel viewModel)
        {
            return;
        }

        if (string.Equals(eventArgs.PropertyName, nameof(MainViewModel.IsCliOpen), StringComparison.Ordinal) && viewModel.IsCliOpen)
        {
            this.cliWindow = new CliWindow();
            this.cliWindow.Closed += this.OnCliWindowClosed;
            this.cliWindow.DataContext = viewModel;
            this.cliWindow.Show(this);
        }

        if (string.Equals(eventArgs.PropertyName, nameof(MainViewModel.IsAddInstanceOpen), StringComparison.Ordinal))
        {
            if (viewModel.IsAddInstanceOpen)
            {
                this.addInstanceWindow = new AddInstanceWindow();
                this.addInstanceWindow.Closed += this.OnAddInstanceWindowClosed;
                this.addInstanceWindow.DataContext = viewModel;
                _ = this.addInstanceWindow.ShowDialog(this);
            }
            else
            {
                this.addInstanceWindow?.Close();
            }
        }
    }

    private void OnAddInstanceWindowClosed(object? sender, EventArgs eventArgs)
    {
        if (sender is AddInstanceWindow closedWindow)
        {
            closedWindow.Closed -= this.OnAddInstanceWindowClosed;
        }

        this.addInstanceWindow = null;
    }

    private void OnCliWindowClosed(object? sender, EventArgs eventArgs)
    {
        if (sender is CliWindow closedWindow)
        {
            closedWindow.Closed -= this.OnCliWindowClosed;
        }

        this.cliWindow = null;
    }
}
