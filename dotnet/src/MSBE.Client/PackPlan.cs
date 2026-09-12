using System.Text.Json;

namespace MSBE.Client;

/// <summary>A preview the daemon holds until it is executed.</summary>
/// <param name="PlanId">The daemon-held plan ID.</param>
/// <param name="PlanDigest">The digest that binds execution to exactly this preview.</param>
/// <param name="Plan">The preview to display. It is never submitted back.</param>
public sealed record PackPlan(string PlanId, string PlanDigest, JsonElement Plan);
