using Avalonia;
using Avalonia.Controls.ApplicationLifetimes;
using Avalonia.Markup.Xaml;

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
            desktop.MainWindow = new MainWindow { DataContext = new MainViewModel() };
        }

        base.OnFrameworkInitializationCompleted();
    }
}
