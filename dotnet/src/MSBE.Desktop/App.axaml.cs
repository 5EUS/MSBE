using Avalonia;
using Avalonia.Controls.ApplicationLifetimes;
using Avalonia.Markup.Xaml;

using MSBE.Client;
using MSBE.Desktop.Services;
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
            var viewModel = new MainViewModel(new UnixSocketMsbeClient(), new TopLevelFolderLauncher(() => desktop.MainWindow));
            desktop.MainWindow = new MainWindow { DataContext = viewModel };

            // macOS delivers the links the application bundle declares as activations, not arguments.
            if (this.TryGetFeature<IActivatableLifetime>() is { } activatable)
            {
                activatable.Activated += async (_, activated) =>
                {
                    if (activated is ProtocolActivatedEventArgs { Kind: ActivationKind.OpenUri } opened)
                    {
                        await viewModel.ReceiveLinkAsync(opened.Uri).ConfigureAwait(true);
                    }
                };
            }

            _ = viewModel.ConnectAsync();
        }

        base.OnFrameworkInitializationCompleted();
    }
}
