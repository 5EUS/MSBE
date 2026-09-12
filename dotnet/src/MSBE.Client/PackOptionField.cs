namespace MSBE.Client;

/// <summary>One field of a codec option schema.</summary>
/// <param name="Key">The stable option key.</param>
/// <param name="Label">The display label.</param>
/// <param name="Description">Explanatory text.</param>
/// <param name="Kind">The closed value shape: <c>boolean</c>, <c>integer</c>, <c>text</c>, <c>choice</c>, <c>multi_choice</c> or <c>path</c>.</param>
/// <param name="Choices">The values a choice accepts; empty for other kinds.</param>
public sealed record PackOptionField(string Key, string Label, string Description, string Kind, IReadOnlyList<PackOptionChoice> Choices);
