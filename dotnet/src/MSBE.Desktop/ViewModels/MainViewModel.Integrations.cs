using System.Collections.ObjectModel;
using System.Net.Sockets;
using System.Text.Json;

using CommunityToolkit.Mvvm.ComponentModel;
using CommunityToolkit.Mvvm.Input;

using MSBE.Client;
using MSBE.Desktop.Resources;

namespace MSBE.Desktop.ViewModels;

/// <content>
/// Settings → Link handlers, Browser component and External tools: how provider links, pages and
/// tool programs reach MSBE, and opening pages in the user's own browser.
/// </content>
internal sealed partial class MainViewModel
{
    /// <summary>The JSON-RPC code of a method the daemon does not serve.</summary>
    private const int MethodNotFoundCode = -32601;

    /// <summary>Gets every link scheme an enabled provider hands links over in.</summary>
    public ObservableCollection<HandlerItem> LinkHandlers { get; } = [];

    /// <summary>Gets or sets why no link handler is listed, or empty when they are.</summary>
    [ObservableProperty]
    [NotifyPropertyChangedFor(nameof(HasLinkHandlersStatus))]
    public partial string LinkHandlersStatus { get; set; } = string.Empty;

    /// <summary>Gets every enabled tool provider and the program registered for it.</summary>
    public ObservableCollection<ToolItem> ExternalTools { get; } = [];

    /// <summary>Gets or sets why no tool provider is listed, or empty when they are.</summary>
    [ObservableProperty]
    [NotifyPropertyChangedFor(nameof(HasExternalToolsStatus))]
    public partial string ExternalToolsStatus { get; set; } = string.Empty;

    /// <summary>Gets or sets a value indicating whether the MSBE browser component is installed beside the daemon.</summary>
    [ObservableProperty]
    [NotifyPropertyChangedFor(nameof(BrowserComponentStatus))]
    [NotifyPropertyChangedFor(nameof(CanOpenWaitingPages))]
    public partial bool IsBrowserComponentInstalled { get; set; } = true;

    /// <summary>Gets a value indicating whether there is something to say about the link handlers.</summary>
    public bool HasLinkHandlersStatus => this.LinkHandlersStatus.Length > 0;

    /// <summary>Gets a value indicating whether there is something to say about the external tools.</summary>
    public bool HasExternalToolsStatus => this.ExternalToolsStatus.Length > 0;

    /// <summary>Gets whether the MSBE browser component is installed, and what that means for waiting pages.</summary>
    public string BrowserComponentStatus => this.IsBrowserComponentInstalled ? Strings.BrowserComponentInstalled : Strings.BrowserComponentMissing;

    /// <summary>Reads what Settings and Browse show about providers from a daemon that owns a download queue.</summary>
    private async Task LoadIntegrationsAsync()
    {
        await this.LoadProvidersAsync().ConfigureAwait(true);
        await this.LoadAccountsAsync().ConfigureAwait(true);
        await this.LoadLinkHandlersAsync().ConfigureAwait(true);
        await this.LoadExternalToolsAsync().ConfigureAwait(true);
    }

    [RelayCommand]
    private async Task OpenWebPageAsync(string? page)
    {
        if (!Uri.TryCreate(page, UriKind.Absolute, out Uri? uri) || !string.Equals(uri.Scheme, Uri.UriSchemeHttps, StringComparison.OrdinalIgnoreCase))
        {
            this.StatusMessage = Strings.WebPageNotHttps;
            return;
        }

        bool opened = this.links is not null && await this.links.OpenAsync(uri).ConfigureAwait(true);
        this.StatusMessage = opened ? Strings.FormatWebPageOpened(uri.Host) : Strings.FormatWebPageNotOpened(uri.AbsoluteUri);
    }

    [RelayCommand]
    private async Task LoadLinkHandlersAsync()
    {
        try
        {
            IReadOnlyList<HandlerStatusInfo> handlers = await this.client.GetHandlerStatusAsync(CancellationToken.None).ConfigureAwait(true);
            this.LinkHandlers.Clear();
            foreach (HandlerStatusInfo handler in handlers)
            {
                this.LinkHandlers.Add(new HandlerItem(handler));
            }

            this.LinkHandlersStatus = handlers.Count == 0 ? Strings.LinkHandlersNone : string.Empty;
        }
        catch (MsbeRpcException exception) when (exception.Code == MethodNotFoundCode)
        {
            this.LinkHandlersStatus = Strings.LinkHandlersUnsupported;
        }
        catch (Exception exception) when (exception is IOException or SocketException or JsonException or InvalidOperationException or KeyNotFoundException)
        {
            this.LinkHandlersStatus = Strings.FormatLinkHandlersFailed(exception.Message);
        }
    }

    [RelayCommand]
    private Task RegisterLinkHandlerAsync(HandlerItem? handler) => handler is null ? Task.CompletedTask : this.ChangeLinkHandlerAsync(handler, replace: false);

    [RelayCommand]
    private Task ConfirmLinkHandlerReplaceAsync(HandlerItem? handler) => handler is null ? Task.CompletedTask : this.ChangeLinkHandlerAsync(handler, replace: true);

    /// <summary>Registers MSBE for a scheme, asking first when another application opens its links.</summary>
    private async Task ChangeLinkHandlerAsync(HandlerItem handler, bool replace)
    {
        handler.Error = string.Empty;
        try
        {
            handler.Update(await this.client.RegisterHandlerAsync(handler.Scheme, replace, CancellationToken.None).ConfigureAwait(true));
            this.StatusMessage = Strings.FormatHandlerRegisteredStatus(handler.Scheme);
        }
        catch (MsbeRpcException exception) when (exception.Code == HandlerRpc.OwnedByAnotherApplication)
        {
            handler.ReplaceText = Strings.FormatHandlerReplaceConfirm(handler.OtherOwner ?? Strings.HandlerOtherApplication, handler.Scheme);
            handler.IsConfirmingReplace = true;
        }
        catch (Exception exception) when (exception is IOException or SocketException or JsonException or InvalidOperationException or KeyNotFoundException)
        {
            handler.Error = exception.Message;
        }
    }

    [RelayCommand]
    private async Task UnregisterLinkHandlerAsync(HandlerItem? handler)
    {
        if (handler is null)
        {
            return;
        }

        handler.Error = string.Empty;
        try
        {
            handler.Update(await this.client.UnregisterHandlerAsync(handler.Scheme, CancellationToken.None).ConfigureAwait(true));
            this.StatusMessage = Strings.FormatHandlerUnregisteredStatus(handler.Scheme);
        }
        catch (Exception exception) when (exception is IOException or SocketException or JsonException or InvalidOperationException or KeyNotFoundException)
        {
            handler.Error = exception.Message;
        }
    }

    [RelayCommand]
    private async Task RefreshBrowserComponentAsync()
    {
        try
        {
            this.ApplyBrowserStatus(await this.client.GetBrowserStatusAsync(CancellationToken.None).ConfigureAwait(true));
        }
        catch (Exception exception) when (exception is IOException or SocketException or JsonException or InvalidOperationException or KeyNotFoundException)
        {
            this.StatusMessage = Strings.FormatBrowserComponentFailed(exception.Message);
        }
    }

    [RelayCommand]
    private async Task LoadExternalToolsAsync()
    {
        try
        {
            IReadOnlyList<ToolStatusInfo> tools = await this.client.ListToolsAsync(CancellationToken.None).ConfigureAwait(true);
            this.ExternalTools.Clear();
            foreach (ToolStatusInfo tool in tools)
            {
                this.ExternalTools.Add(new ToolItem(tool));
            }

            this.ExternalToolsStatus = tools.Count == 0 ? Strings.ToolsNone : string.Empty;
        }
        catch (MsbeRpcException exception) when (exception.Code == MethodNotFoundCode)
        {
            this.ExternalToolsStatus = Strings.ToolsUnsupported;
        }
        catch (Exception exception) when (exception is IOException or SocketException or JsonException or InvalidOperationException or KeyNotFoundException)
        {
            this.ExternalToolsStatus = Strings.FormatToolsFailed(exception.Message);
        }
    }

    [RelayCommand]
    private Task RegisterExternalToolAsync(ToolItem? tool) => tool is not { CanRegister: true }
        ? Task.CompletedTask
        : this.ChangeExternalToolAsync(
            tool,
            () => this.client.RegisterToolAsync(tool.Provider, tool.ProgramPath.Trim(), CancellationToken.None),
            Strings.FormatToolRegisteredStatus(tool.Name));

    [RelayCommand]
    private Task ForgetExternalToolAsync(ToolItem? tool) => tool is not { HasProgram: true }
        ? Task.CompletedTask
        : this.ChangeExternalToolAsync(
            tool,
            () => this.client.ForgetToolAsync(tool.Provider, CancellationToken.None),
            Strings.FormatToolForgottenStatus(tool.Name));

    /// <summary>Asks the daemon to change a tool registration, then shows the provider as it now is.</summary>
    private async Task ChangeExternalToolAsync(ToolItem tool, Func<Task<ToolStatusInfo>> change, string done)
    {
        tool.Error = string.Empty;
        try
        {
            tool.Update(await change().ConfigureAwait(true));
            this.StatusMessage = done;
        }
        catch (Exception exception) when (exception is IOException or SocketException or JsonException or InvalidOperationException or KeyNotFoundException)
        {
            tool.Error = exception.Message;
        }
    }
}
