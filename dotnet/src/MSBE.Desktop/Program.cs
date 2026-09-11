using Avalonia;

namespace MSBE.Desktop;

/// <summary>Process entry point.</summary>
internal static class Program
{
    /// <summary>Starts the desktop application.</summary>
    /// <param name="args">Command-line arguments forwarded to Avalonia.</param>
    [STAThread]
    public static void Main(string[] args) =>
        BuildAvaloniaApp().StartWithClassicDesktopLifetime(args);

    /// <summary>Configures the Avalonia application. Referenced by the XAML previewer.</summary>
    /// <returns>The configured application builder.</returns>
    public static AppBuilder BuildAvaloniaApp() =>
        AppBuilder.Configure<App>()
            .UsePlatformDetect()
            .WithInterFont()
            .LogToTrace();
}
