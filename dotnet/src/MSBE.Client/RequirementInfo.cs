namespace MSBE.Client;

/// <summary>A project one mod requires or declares incompatible.</summary>
/// <param name="Provider">The provider of the project referred to.</param>
/// <param name="Project">The project referred to.</param>
/// <param name="DeclaredBy">The mod that declared it.</param>
public sealed record RequirementInfo(string Provider, string Project, string DeclaredBy);
