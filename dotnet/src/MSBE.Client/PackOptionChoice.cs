namespace MSBE.Client;

/// <summary>One value a choice option accepts.</summary>
/// <param name="Value">The serialized value.</param>
/// <param name="Label">The display label.</param>
public sealed record PackOptionChoice(string Value, string Label);
