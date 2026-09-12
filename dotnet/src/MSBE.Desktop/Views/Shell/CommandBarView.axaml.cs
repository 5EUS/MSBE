using System.Diagnostics.CodeAnalysis;

using Avalonia.Controls;
using Avalonia.Input;
using Avalonia.Interactivity;

namespace MSBE.Desktop.Views.Shell;

/// <summary>Hosts shell menus and global actions.</summary>
[SuppressMessage("Design", "CA1515:Consider making public types internal", Justification = "Avalonia's external previewer must instantiate the view.")]
public partial class CommandBarView : UserControl
{
    /// <summary>Initializes a new instance of the <see cref="CommandBarView" /> class.</summary>
    public CommandBarView()
    {
        this.InitializeComponent();
        this.TitleBarDragRegion.PointerPressed += this.OnTitleBarPointerPressed;
        this.TitleBarDragRegion.DoubleTapped += this.OnTitleBarDoubleTapped;
        this.MinimizeButton.Click += this.OnMinimizeClick;
        this.MaximizeButton.Click += this.OnMaximizeClick;
        this.CloseButton.Click += this.OnCloseClick;
    }

    private Window? OwnerWindow => TopLevel.GetTopLevel(this) as Window;

    private void OnTitleBarPointerPressed(object? sender, PointerPressedEventArgs eventArgs)
    {
        if (this.OwnerWindow is { } window && eventArgs.GetCurrentPoint(this).Properties.IsLeftButtonPressed)
        {
            window.BeginMoveDrag(eventArgs);
        }
    }

    private void OnTitleBarDoubleTapped(object? sender, TappedEventArgs eventArgs)
    {
        this.ToggleMaximized();
        eventArgs.Handled = true;
    }

    private void OnMinimizeClick(object? sender, RoutedEventArgs eventArgs)
    {
        if (this.OwnerWindow is { } window)
        {
            window.WindowState = WindowState.Minimized;
        }
    }

    private void OnMaximizeClick(object? sender, RoutedEventArgs eventArgs) => this.ToggleMaximized();

    private void OnCloseClick(object? sender, RoutedEventArgs eventArgs) => this.OwnerWindow?.Close();

    private void ToggleMaximized()
    {
        if (this.OwnerWindow is { } window)
        {
            window.WindowState = window.WindowState == WindowState.Maximized
                ? WindowState.Normal
                : WindowState.Maximized;
        }
    }
}
