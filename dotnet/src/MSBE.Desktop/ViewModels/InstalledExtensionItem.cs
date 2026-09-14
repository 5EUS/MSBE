using MSBE.Client;
using MSBE.Desktop.Resources;

namespace MSBE.Desktop.ViewModels;

/// <summary>An installed extension as Settings shows it.</summary>
/// <param name="Title">The extension ID and version, or the file name when its envelope could not be read.</param>
/// <param name="Kind">What kind of extension it is.</param>
/// <param name="Status"><c>Active</c> or <c>Refused</c>.</param>
/// <param name="Detail">Its signer and file, or why it was refused.</param>
/// <param name="IsActive">Whether it runs.</param>
internal sealed record InstalledExtensionItem(string Title, string Kind, string Status, string Detail, bool IsActive)
{
    /// <summary>Describes an extension the daemon reported.</summary>
    /// <param name="extension">The extension.</param>
    /// <returns>Its display text.</returns>
    public static InstalledExtensionItem From(ExtensionInfo extension)
    {
        ArgumentNullException.ThrowIfNull(extension);
        string title = extension.Id is null
            ? Path.GetFileName(extension.Path)
            : $"{extension.Id} {extension.Version}".TrimEnd();
        string kind = extension.Kind switch
        {
            "codec" => Strings.ExtensionKindCodec,
            "program" => Strings.ExtensionKindProgram,
            "trust_root" => Strings.ExtensionKindTrustRoot,
            _ => extension.Kind,
        };
        if (!extension.IsActive)
        {
            return new InstalledExtensionItem(title, kind, Strings.ExtensionRefused, Strings.FormatExtensionRefusedDetail(extension.Reason ?? Strings.ExtensionRefusedReason, extension.Path), IsActive: false);
        }

        string detail = extension.Signer is null ? extension.Path : Strings.FormatExtensionSignedBy(extension.Signer, extension.Path);
        return new InstalledExtensionItem(title, kind, Strings.ExtensionActive, detail, IsActive: true);
    }
}
