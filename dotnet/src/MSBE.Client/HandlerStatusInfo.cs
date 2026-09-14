namespace MSBE.Client;

/// <summary>Which application opens a provider's link scheme for the current user.</summary>
/// <param name="Scheme">The scheme, lowercase and without <c>://</c>.</param>
/// <param name="Provider">The enabled provider whose links use the scheme, if one does.</param>
/// <param name="Owner"><c>nobody</c>, <c>msbe</c> or <c>other</c>.</param>
/// <param name="OwnerName">The other application, as the platform names it.</param>
/// <param name="IsCurrent">Whether MSBE's registration opens links with this installation.</param>
/// <param name="Previous">The application MSBE replaced, which unregistering gives the scheme back to.</param>
public sealed record HandlerStatusInfo(string Scheme, string? Provider, string Owner, string? OwnerName, bool IsCurrent, string? Previous);
