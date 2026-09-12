using Avalonia;
using Avalonia.Controls.ApplicationLifetimes;
using Avalonia.Markup.Xaml;

using MSBE.Client;
using MSBE.Desktop.ViewModels;
using MSBE.Desktop.Views.Shell;

namespace MSBE.Desktop;

/// <summary>The Avalonia application.</summary>
internal partial class App : Application
{
    /// <inheritdoc />
    public override void Initialize() => AvaloniaXamlLoader.Load(this);

    /// <inheritdoc />
    public override void OnFrameworkInitializationCompleted()
    {
        if (this.ApplicationLifetime is IClassicDesktopStyleApplicationLifetime desktop)
        {
            var viewModel = new MainViewModel(new UnixSocketMsbeClient());
            desktop.MainWindow = new MainWindow { DataContext = viewModel };
            _ = viewModel.ConnectAsync();
        }

        base.OnFrameworkInitializationCompleted();
    }
}
