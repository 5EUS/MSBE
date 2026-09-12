using System.Buffers;
using System.Globalization;
using System.Text.Json;

using CommunityToolkit.Mvvm.ComponentModel;

using MSBE.Client;

namespace MSBE.Desktop.ViewModels;

/// <summary>One codec option rendered with a native control from the daemon's schema.</summary>
internal sealed partial class PackOptionItem : ObservableObject
{
    private readonly JsonElement original;

    /// <summary>Initializes a new instance of the <see cref="PackOptionItem" /> class.</summary>
    /// <param name="field">The schema field.</param>
    /// <param name="value">The field's normalized value.</param>
    public PackOptionItem(PackOptionField field, JsonElement value)
    {
        ArgumentNullException.ThrowIfNull(field);
        this.Key = field.Key;
        this.Label = field.Label;
        this.Description = field.Description;
        this.Kind = field.Kind;
        this.Choices = field.Choices;
        this.original = value.ValueKind == JsonValueKind.Undefined ? value : value.Clone();
        switch (value.ValueKind)
        {
            case JsonValueKind.True or JsonValueKind.False:
                this.BooleanValue = value.GetBoolean();
                break;
            case JsonValueKind.String:
                string text = value.GetString() ?? string.Empty;
                this.TextValue = text;
                this.SelectedChoice = field.Choices.FirstOrDefault(choice => string.Equals(choice.Value, text, StringComparison.Ordinal));
                break;
            case JsonValueKind.Number:
                this.TextValue = value.GetRawText();
                break;
            default:
                break;
        }
    }

    /// <summary>Gets the stable option key.</summary>
    public string Key { get; }

    /// <summary>Gets the display label.</summary>
    public string Label { get; }

    /// <summary>Gets the explanatory text.</summary>
    public string Description { get; }

    /// <summary>Gets the schema's value shape.</summary>
    public string Kind { get; }

    /// <summary>Gets the values a choice accepts.</summary>
    public IReadOnlyList<PackOptionChoice> Choices { get; }

    /// <summary>Gets a value indicating whether the option is a check box.</summary>
    public bool IsBoolean => string.Equals(this.Kind, "boolean", StringComparison.Ordinal);

    /// <summary>Gets a value indicating whether the option is a closed choice.</summary>
    public bool IsChoice => string.Equals(this.Kind, "choice", StringComparison.Ordinal);

    /// <summary>Gets a value indicating whether the option is edited as text.</summary>
    public bool IsText => this.Kind is "text" or "path" or "integer";

    /// <summary>Gets or sets the value of a boolean option.</summary>
    [ObservableProperty]
    public partial bool BooleanValue { get; set; }

    /// <summary>Gets or sets the selected value of a choice option.</summary>
    [ObservableProperty]
    public partial PackOptionChoice? SelectedChoice { get; set; }

    /// <summary>Gets or sets the value of a text, path or integer option.</summary>
    [ObservableProperty]
    public partial string TextValue { get; set; } = string.Empty;

    /// <summary>The option's current value as plain JSON, in the shape the daemon's schema reads.</summary>
    /// <returns>The value element.</returns>
    public JsonElement ToJson() => this.Kind switch
    {
        "boolean" => Element(writer => writer.WriteBooleanValue(this.BooleanValue)),
        "choice" when this.SelectedChoice is not null => Element(writer => writer.WriteStringValue(this.SelectedChoice.Value)),
        "text" or "path" => Element(writer => writer.WriteStringValue(this.TextValue)),
        "integer" when long.TryParse(this.TextValue, NumberStyles.Integer, CultureInfo.InvariantCulture, out long number) => Element(writer => writer.WriteNumberValue(number)),
        _ => this.original,
    };

    private static JsonElement Element(Action<Utf8JsonWriter> write)
    {
        var buffer = new ArrayBufferWriter<byte>();
#pragma warning disable MA0045 // Utf8JsonWriter does not implement IAsyncDisposable.
        using (var writer = new Utf8JsonWriter(buffer))
        {
            write(writer);
        }
#pragma warning restore MA0045

        using JsonDocument document = JsonDocument.Parse(buffer.WrittenMemory);
        return document.RootElement.Clone();
    }
}
