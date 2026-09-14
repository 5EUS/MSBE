namespace MSBE.Desktop.Services;

/// <summary>Opens web pages in the user's own browser.</summary>
internal interface ILinkLauncher
{
    /// <summary>Opens an HTTPS page in the platform's default browser.</summary>
    /// <param name="page">The page.</param>
    /// <returns><see langword="true" /> if the platform opened the page.</returns>
    Task<bool> OpenAsync(Uri page);
}
