namespace MSBE.Desktop.Services;

/// <summary>Shows folders in the platform's file manager.</summary>
internal interface IFolderLauncher
{
    /// <summary>Opens a folder in the platform's file manager.</summary>
    /// <param name="path">The absolute path of the folder.</param>
    /// <returns><see langword="true" /> if the platform opened the folder.</returns>
    Task<bool> OpenAsync(string path);
}
