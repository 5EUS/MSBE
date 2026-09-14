using System.Buffers;
using System.Text.Json;

using MSBE.Client;
using MSBE.Desktop.Resources;
using MSBE.Desktop.Services;
using MSBE.Desktop.ViewModels;

using Xunit;

namespace MSBE.Desktop.Tests;

/// <summary>Tests for <see cref="MainViewModel" />.</summary>
public sealed class MainViewModelTests
{
    private const string DataDirectory = "/data/msbe";
    private const string IrisQueuedJson = """{"id":1,"revision":1,"title":"Iris","source":"modrinth:iris","target":{"instance":"alpha","profile":"default"},"with_deps":true,"attempts":0,"state":{"kind":"queued"},"files":[]}""";
    private const string EmptyQueueJson = """{"next":0,"paused":false,"order":[],"items":[]}""";
    private const string DownloadTarget = "\"target\":{\"instance\":\"alpha\",\"profile\":\"default\"}";
    private const string QueueJson = $$$"""
        {"next":3,"paused":false,"order":[1,2,3],"items":[
          {"id":1,"revision":1,"title":"Sodium","source":"modrinth:sodium",{{{DownloadTarget}}},"attempts":1,"state":{"kind":"downloading"},"files":[]},
          {"id":2,"revision":2,"title":"Iris","source":"modrinth:iris",{{{DownloadTarget}}},"attempts":0,"state":{"kind":"queued"},"files":[]},
          {"id":3,"revision":3,"title":"Tool","source":"assisted:tool",{{{DownloadTarget}}},"attempts":1,"state":{"kind":"awaiting_user","page":"https://www.example.test/tool","scheme":"handoff"},"files":[]}
        ]}
        """;

    private const string FinishedQueueJson = $$$"""
        {"next":7,"paused":true,"order":[1,2,3,4],"items":[
          {"id":1,"revision":4,"title":"Sodium","source":"modrinth:sodium",{{{DownloadTarget}}},"attempts":1,"state":{"kind":"completed"},"files":[],"added":["sodium"]},
          {"id":2,"revision":5,"title":"Iris","source":"modrinth:iris",{{{DownloadTarget}}},"attempts":0,"state":{"kind":"cancelled"},"files":[]},
          {"id":3,"revision":6,"title":"Tool","source":"assisted:tool",{{{DownloadTarget}}},"attempts":1,"state":{"kind":"failed","message":"the link expired"},"files":[]},
          {"id":4,"revision":7,"attempts":1,"state":{"kind":"downloaded"},"files":[{"provider":"assisted","project":"gear","release":"r9","name":"gear.zip","state":{"kind":"downloaded"}}]}
        ]}
        """;

    /// <summary>Refresh loads, sorts and selects registered instances.</summary>
    /// <returns>A task representing the test.</returns>
    [Fact]
    public async Task RefreshInstancesLoadsRegisteredInstances()
    {
        const string StatusJson = """{"name":"alpha","root":"/games/alpha","plan_id":"minecraft","plan_version":"1","loader":"fabric","game_version":"1.21.1","deployed_profile":"default","deployed_files":12}""";
        var client = new TestClient(arguments => arguments.Contains("list", StringComparer.Ordinal)
            ? new CommandResult(0, "[\"zeta\",\"alpha\"]", string.Empty)
            : new CommandResult(0, StatusJson, string.Empty));
        MainViewModel vm = new(client);

        await vm.RefreshInstancesCommand.ExecuteAsync(parameter: null);

        Assert.Equal(["alpha", "zeta"], vm.Instances);
        Assert.Equal("alpha", vm.SelectedInstance);
        Assert.Equal("/games/alpha", vm.SelectedInstanceRoot);
        Assert.Equal("minecraft", vm.SelectedGameName);
        Assert.Equal("1.21.1 · fabric", vm.SelectedInstanceTarget);
        Assert.Equal("default · 12 managed files", vm.SelectedInstanceDeployment);
        Assert.Equal("2 registered instance(s).", vm.StatusMessage);

        vm.InstanceSearchText = "zet";

        Assert.Equal(["zeta"], vm.Instances);
    }

    /// <summary>Refresh exposes daemon failures as recoverable page state.</summary>
    /// <returns>A task representing the test.</returns>
    [Fact]
    public async Task RefreshInstancesReportsDaemonFailure()
    {
        MainViewModel vm = new(new TestClient(_ => new CommandResult(1, string.Empty, "instance store unavailable")));

        await vm.RefreshInstancesCommand.ExecuteAsync(parameter: null);

        Assert.True(vm.HasInstanceError);
        Assert.Equal("instance store unavailable", vm.InstanceError);
        Assert.Equal("Could not load instances.", vm.StatusMessage);
    }

    /// <summary>Instance removal uses the structured command and refreshes the library.</summary>
    /// <returns>A task representing the test.</returns>
    [Fact]
    public async Task RemoveSelectedInstanceUnregistersAndRefreshesTheLibrary()
    {
        List<IReadOnlyList<string>> calls = [];
        var client = new TestClient(arguments =>
        {
            calls.Add(arguments);
            return arguments.Contains("list", StringComparer.Ordinal)
                ? new CommandResult(0, "[]", string.Empty)
                : new CommandResult(0, string.Empty, string.Empty);
        });
        MainViewModel vm = new(client) { SelectedInstance = "alpha" };

        await vm.RemoveSelectedInstanceCommand.ExecuteAsync(parameter: null);

        Assert.Contains(calls, arguments => arguments.SequenceEqual(
            ["--format", "json", "instance", "remove", "alpha"],
            StringComparer.Ordinal));
        Assert.Null(vm.SelectedInstance);
        Assert.Empty(vm.Instances);
        Assert.Equal("Removed instance alpha. Its game files remain in place.", vm.StatusMessage);
    }

    /// <summary>Native registration sends structured arguments and selects the new instance.</summary>
    /// <returns>A task representing the test.</returns>
    [Fact]
    public async Task AddInstanceRegistersAndSelectsInstance()
    {
        const string StatusJson = """{"name":"alpha","root":"/games/alpha","plan_id":"minecraft","plan_version":"1","loader":"fabric","game_version":"1.21.1","deployed_profile":null,"deployed_files":0}""";
        List<IReadOnlyList<string>> calls = [];
        var client = new TestClient(arguments =>
        {
            calls.Add(arguments);
            if (arguments.Contains("list", StringComparer.Ordinal))
            {
                return new CommandResult(0, "[\"alpha\"]", string.Empty);
            }

            return new CommandResult(0, StatusJson, string.Empty);
        });
        MainViewModel vm = new(client)
        {
            IsAddInstanceOpen = true,
            NewInstanceName = "alpha",
            NewInstanceRoot = "/games/alpha",
            NewInstanceGame = new GameInfo("minecraft", "Minecraft", "1", ["fabric"]),
            NewInstanceLoader = "fabric",
            NewInstanceSide = "Client",
            NewInstanceGameVersion = "1.21.1",
        };

        await vm.SubmitAddInstanceCommand.ExecuteAsync(parameter: null);

        string[] expectedArguments =
        [
            "--format", "json", "instance", "add", "alpha",
            "--root", "/games/alpha", "--game", "minecraft",
            "--loader", "fabric", "--side", "client", "--game-version", "1.21.1",
        ];
        Assert.Contains(calls, arguments => arguments.SequenceEqual(expectedArguments, StringComparer.Ordinal));
        Assert.False(vm.IsAddInstanceOpen);
        Assert.Equal("alpha", vm.SelectedInstance);
        Assert.False(vm.HasAddInstanceError);
    }

    /// <summary>The instance wizard filters supported games by name and ecosystem.</summary>
    [Fact]
    public void GameSearchFiltersSupportedGames()
    {
        MainViewModel vm = new(new TestClient(_ => new CommandResult(0, string.Empty, string.Empty)));
        vm.Games.Add(new GameInfo("minecraft", "Minecraft", "1", ["fabric", "neoforge"]));
        vm.Games.Add(new GameInfo("nomanssky", "No Man's Sky", "1", ["native"]));

        vm.AddInstanceCommand.Execute(parameter: null);
        vm.GameSearchText = "neo";

        Assert.Equal("Minecraft", Assert.Single(vm.FilteredGames).Name);
        Assert.False(vm.IsGameSearchEmpty);

        vm.GameSearchText = "stardew";

        Assert.Empty(vm.FilteredGames);
        Assert.True(vm.IsGameSearchEmpty);
    }

    /// <summary>The selected instance loads its deployed profile and ordered mods.</summary>
    [Fact]
    public void SelectedInstanceLoadsOrderedMods()
    {
        const string StatusJson = """{"name":"alpha","root":"/games/alpha","plan_id":"minecraft","plan_version":"1","loader":"fabric","game_version":"1.21.1","deployed_profile":"default","deployed_files":2}""";
        const string ProfilesJson = """{"profiles":["testing","default"],"deployed":"default"}""";
        const string ModsJson = """{"target":{"loader":"fabric","loader_version":"0.16.10","side":"client"},"order":["iris"],"components":{},"mods":{"iris":{"origin":"iris.jar","provider":{"provider":"modrinth","project":"iris","version":"v1","version_number":"1.8.0","hashes":{}},"files":[{"source":"iris.jar","blob":"sha256:01"}]},"sodium":{"origin":"sodium.jar","files":[{"source":"sodium.jar","blob":"sha256:02"}]}}}""";
        var client = new TestClient(arguments =>
        {
            if (arguments.Contains("status", StringComparer.Ordinal))
            {
                return new CommandResult(0, StatusJson, string.Empty);
            }

            if (arguments.Contains("list", StringComparer.Ordinal))
            {
                return new CommandResult(0, ProfilesJson, string.Empty);
            }

            return new CommandResult(0, ModsJson, string.Empty);
        });
        MainViewModel vm = new(client);

        vm.SelectedInstance = "alpha";

        Assert.Equal("default", vm.SelectedProfile);
        Assert.Equal(["iris", "sodium"], vm.Mods.Select(mod => mod.Name));
        Assert.Equal("modrinth", vm.Mods[0].Source);
        Assert.Equal("1.8.0", vm.Mods[0].Version);
        Assert.Equal("Local file", vm.Mods[1].Source);
        Assert.Equal("fabric 0.16.10 · Client", vm.SelectedProfileTargetSummary);
        Assert.False(vm.IsModsEmpty);
    }

    /// <summary>Deployment review preserves operation order and explains withheld files.</summary>
    /// <returns>A task representing the test.</returns>
    [Fact]
    public async Task PreviewDeploymentLoadsOperationsAndExclusions()
    {
        const string StatusJson = """{"name":"alpha","root":"/games/alpha","plan_id":"minecraft","plan_version":"1","loader":"fabric","game_version":"1.21.1","deployed_profile":"default","deployed_files":2}""";
        const string ProfilesJson = """{"profiles":["default"],"deployed":"default"}""";
        const string ModsJson = """{"order":[],"components":{},"mods":{}}""";
        const string PlanJson = """{"profile":"default","operations":[{"op":"create_dir","path":"mods"},{"op":"materialize","path":"mods/iris.jar","blob":"sha256:01","mutable":false},{"op":"remove","path":"mods/old.jar"}],"unchanged":7,"kept":["config/iris.properties"],"excluded":[{"module":"iris","file":{"source":"debug/iris.pdb","reason":{"kind":"quarantined","pattern":"**/*.pdb"}}}]}""";
        List<IReadOnlyList<string>> calls = [];
        var client = new TestClient(arguments =>
        {
            calls.Add(arguments);
            if (arguments.Contains("status", StringComparer.Ordinal))
            {
                return new CommandResult(0, StatusJson, string.Empty);
            }

            if (arguments.Contains("list", StringComparer.Ordinal))
            {
                return new CommandResult(0, ProfilesJson, string.Empty);
            }

            if (arguments.Contains("deploy", StringComparer.Ordinal))
            {
                return new CommandResult(0, PlanJson, string.Empty);
            }

            return new CommandResult(0, ModsJson, string.Empty);
        });
        MainViewModel vm = new(client)
        {
            SelectedInstance = "alpha",
        };

        await vm.PreviewDeploymentCommand.ExecuteAsync(parameter: null);

        string[] expectedArguments = ["--format", "json", "deploy", "alpha", "--profile", "default", "--dry-run"];
        Assert.Contains(calls, arguments => arguments.SequenceEqual(expectedArguments, StringComparer.Ordinal));
        Assert.True(vm.IsDeploymentPreviewOpen);
        Assert.Equal(["Create folder", "Place", "Remove"], vm.DeploymentChanges.Select(change => change.Action));
        Assert.Equal("mods/iris.jar", vm.DeploymentChanges[1].Path);
        Assert.Equal(7, vm.DeploymentUnchangedCount);
        Assert.Equal(1, vm.DeploymentKeptCount);
        DeploymentExclusionItem exclusion = Assert.Single(vm.DeploymentExclusions);
        Assert.Equal("iris", exclusion.Module);
        Assert.Equal("debug/iris.pdb", exclusion.Source);
        Assert.Equal("quarantined - **/*.pdb", exclusion.Reason);
        Assert.False(vm.HasDeploymentError);
    }

    /// <summary>Browse searches the selected target and installs marked provider results in bulk.</summary>
    /// <returns>A task representing the test.</returns>
    [Fact]
    public async Task BrowseSearchesAndAddsWithDependencies()
    {
        const string StatusJson = """{"name":"alpha","root":"/games/alpha","plan_id":"minecraft","plan_version":"1","loader":"fabric","game_version":"1.21.1","deployed_profile":"default","deployed_files":2}""";
        const string ProfilesJson = """{"profiles":["default"],"deployed":"default"}""";
        const string ModsJson = """{"order":["sodium"],"components":{},"mods":{"sodium":{"origin":"sodium.jar","provider":{"provider":"modrinth","project":"AANobbMI","version":"v1","version_number":"1.0","hashes":{}},"files":[]}}}""";
        const string SearchJson = """[{"provider":"modrinth","project":"AANobbMI","slug":"sodium","title":"Sodium","description":"Rendering optimization","icon_url":"https://cdn.modrinth.com/data/AANobbMI/icon.png","downloads":12000000},{"provider":"modrinth","project":"YL57xq9U","slug":"iris","title":"Iris","description":"Shader support","icon_url":null,"downloads":9000000}]""";
        List<IReadOnlyList<string>> calls = [];
        var client = new TestClient(arguments =>
        {
            calls.Add(arguments);
            if (arguments.Contains("status", StringComparer.Ordinal))
            {
                return new CommandResult(0, StatusJson, string.Empty);
            }

            if (arguments.Contains("list", StringComparer.Ordinal))
            {
                return new CommandResult(0, ProfilesJson, string.Empty);
            }

            if (arguments.Contains("search", StringComparer.Ordinal))
            {
                return new CommandResult(0, SearchJson, string.Empty);
            }

            return new CommandResult(0, ModsJson, string.Empty);
        })
        {
            Answer = (method, _) => string.Equals(method, "download.enqueue", StringComparison.Ordinal)
                ? IrisQueuedJson
                : $$"""{"next":1,"paused":false,"order":[1],"items":[{{IrisQueuedJson}}]}""",
        };
        MainViewModel vm = new(client) { SelectedInstance = "alpha", BrowseQuery = "rendering", IsDownloadQueueSupported = true };

        await vm.SearchBrowseCommand.ExecuteAsync(parameter: null);
        vm.SelectedBrowseResult = vm.BrowseResults[0];
        vm.BrowseResults[1].IsMarked = true;

        Assert.True(vm.BrowseResults[0].IsInstalled);
        Assert.True(vm.BrowseResults[0].IsMarked);
        Assert.Equal("1 selected", vm.MarkedBrowseResultCount);
        await vm.AddBrowseResultCommand.ExecuteAsync(parameter: null);

        Assert.Equal("Sodium", vm.SelectedBrowseResult.Title);
        Assert.Equal("https://cdn.modrinth.com/data/AANobbMI/icon.png", vm.SelectedBrowseResult.IconSource);
        Assert.Contains(calls, arguments => arguments.SequenceEqual(
            ["--format", "json", "search", "alpha", "rendering", "--profile", "default", "--limit", "30"],
            StringComparer.Ordinal));
        JsonElement enqueued = client.LastParameters("download.enqueue");
        Assert.Equal("alpha", enqueued.GetProperty("instance").GetString());
        Assert.Equal("default", enqueued.GetProperty("profile").GetString());
        Assert.Equal("modrinth:iris", enqueued.GetProperty("source").GetString());
        Assert.True(enqueued.GetProperty("with_deps").GetBoolean());
        Assert.Equal("Iris", enqueued.GetProperty("title").GetString());
        DownloadQueueItem download = Assert.Single(vm.QueuedDownloads);
        Assert.Equal("Iris", download.Title);
        Assert.Equal(DownloadState.Queued, download.State);
        Assert.Equal("modrinth · alpha / default", download.Summary);
        Assert.Equal("Queued 1 mod(s) for default.", vm.StatusMessage);
        Assert.Empty(vm.MarkedBrowseResults);
    }

    /// <summary>Downloads shows the daemon's queue in order, and sends the queue's controls to the daemon.</summary>
    /// <returns>A task representing the test.</returns>
    [Fact]
    public async Task DownloadsMirrorTheDaemonQueueAndSendItsControls()
    {
        TestClient client = DownloadQueueClient(() => QueueJson);
        MainViewModel vm = new(client) { SelectedInstance = "alpha", IsDownloadQueueSupported = true };

        await vm.RefreshDownloadsCommand.ExecuteAsync(parameter: null);

        Assert.Equal("Sodium", vm.ActiveDownload?.Title);
        Assert.Equal(["Iris", "Tool"], vm.QueuedDownloads.Select(download => download.Title));
        Assert.Equal(3, vm.PendingDownloadCount);
        Assert.Equal("Downloading Sodium · 2 queued", vm.DownloadSummary);
        DownloadQueueItem tool = vm.QueuedDownloads[1];
        Assert.True(tool.IsAwaitingUser);
        Assert.Equal("Start the download at https://www.example.test/tool", tool.Detail);
        Assert.Equal(0L, client.LastParameters("download.list").GetProperty("after").GetInt64());

        await vm.MoveDownloadUpCommand.ExecuteAsync(tool);
        JsonElement moved = client.LastParameters("download.move");
        Assert.Equal(3L, moved.GetProperty("id").GetInt64());
        Assert.Equal(1, moved.GetProperty("position").GetInt32());
        Assert.Equal(3L, client.LastParameters("download.list").GetProperty("after").GetInt64());
        await vm.RemoveQueuedDownloadCommand.ExecuteAsync(vm.QueuedDownloads[0]);
        Assert.Equal(2L, client.LastParameters("download.cancel").GetProperty("id").GetInt64());
        await vm.ToggleDownloadQueuePausedCommand.ExecuteAsync(parameter: null);
        Assert.Equal(JsonValueKind.Null, client.LastParameters("download.pause").ValueKind);
    }

    /// <summary>Downloads announces what the daemon added, retries failures, and adds a link that arrived on its own to the selected profile.</summary>
    /// <returns>A task representing the test.</returns>
    [Fact]
    public async Task DownloadsRetryFailuresAndAddStrayLinksToTheSelectedProfile()
    {
        string queue = QueueJson;
        TestClient client = DownloadQueueClient(() => queue);
        MainViewModel vm = new(client) { SelectedInstance = "alpha", IsDownloadQueueSupported = true };
        await vm.RefreshDownloadsCommand.ExecuteAsync(parameter: null);

        queue = FinishedQueueJson;
        await vm.RefreshDownloadsCommand.ExecuteAsync(parameter: null);

        Assert.Null(vm.ActiveDownload);
        Assert.True(vm.IsDownloadQueuePaused);
        Assert.Equal("Resume queue", vm.DownloadQueueToggleLabel);
        Assert.Equal(["Tool", "Iris", "Sodium"], vm.FinishedDownloads.Select(download => download.Title));
        Assert.Equal("Added 1 mod", vm.FinishedDownloads[2].Detail);
        Assert.Equal("Added Sodium to default. Review deployment to apply it.", vm.StatusMessage);
        DownloadQueueItem stray = Assert.Single(vm.QueuedDownloads);
        Assert.True(stray.NeedsProfile);
        Assert.Equal("gear.zip", stray.Title);
        Assert.Equal("assisted · no profile yet", stray.Summary);
        Assert.Equal("Paused · 1 queued", vm.DownloadSummary);

        DownloadQueueItem failed = vm.FinishedDownloads[0];
        Assert.Equal("the link expired", failed.Detail);
        await vm.RetryDownloadCommand.ExecuteAsync(failed);
        Assert.Equal(3L, client.LastParameters("download.retry").GetProperty("id").GetInt64());
        await vm.AddDownloadToProfileCommand.ExecuteAsync(stray);
        JsonElement confirmed = client.LastParameters("download.confirm");
        Assert.Equal(4L, confirmed.GetProperty("id").GetInt64());
        Assert.Equal("alpha", confirmed.GetProperty("instance").GetString());
        Assert.Equal("default", confirmed.GetProperty("profile").GetString());
        await vm.ToggleDownloadQueuePausedCommand.ExecuteAsync(parameter: null);
        Assert.Equal(JsonValueKind.Null, client.LastParameters("download.resume").ValueKind);

        queue = """{"next":8,"paused":false,"order":[4],"items":[]}""";
        await vm.ClearFinishedDownloadsCommand.ExecuteAsync(parameter: null);

        Assert.Contains(client.Invocations, call => string.Equals(call.Method, "download.clear", StringComparison.Ordinal));
        Assert.False(vm.HasFinishedDownloads);
        Assert.Equal("1 queued", vm.DownloadSummary);
    }

    /// <summary>The MSBE browser controls open a waiting download's page, go to the next page without resending the setting, and close the browser.</summary>
    /// <returns>A task representing the test.</returns>
    [Fact]
    public async Task BrowserControlsDriveTheDaemonBrowser()
    {
        const string Opened = """{"running":true,"provider":"assisted","item":3,"page":"https://www.example.test/tool","position":1,"waiting":2,"title":"Tool files","auto_advance":true}""";
        const string Closed = """{"running":false,"waiting":2,"auto_advance":true,"message":"the MSBE browser was stopped: it reported a download outside its quarantine"}""";
        TestClient client = DownloadQueueClient(() => QueueJson);
        client.Answer = (method, _) => method switch
        {
            "download.list" => QueueJson,
            "browser.open" => Opened,
            "browser.status" or "browser.close" => Closed,
            _ => EmptyQueueJson,
        };
        MainViewModel vm = new(client) { IsDownloadQueueSupported = true, IsBrowserAutoAdvancing = true };

        await vm.RefreshDownloadsCommand.ExecuteAsync(parameter: null);

        Assert.True(vm.CanOpenWaitingPages);
        Assert.Equal("the MSBE browser was stopped: it reported a download outside its quarantine", vm.StatusMessage);
        DownloadQueueItem tool = vm.QueuedDownloads[1];
        await vm.OpenDownloadPageCommand.ExecuteAsync(tool);

        JsonElement opened = client.LastParameters("browser.open");
        Assert.Equal(3L, opened.GetProperty("id").GetInt64());
        Assert.True(opened.GetProperty("auto_advance").GetBoolean());
        Assert.True(vm.IsBrowserOpen);
        Assert.False(vm.CanOpenWaitingPages);
        Assert.Equal("assisted · 1 of 2 · Tool files", vm.BrowserSummary);

        await vm.NextBrowserPageCommand.ExecuteAsync(parameter: null);
        JsonElement next = client.LastParameters("browser.open");
        Assert.False(next.TryGetProperty("id", out _));
        Assert.False(next.TryGetProperty("auto_advance", out _));

        await vm.CloseMsbeBrowserCommand.ExecuteAsync(parameter: null);
        Assert.False(vm.IsBrowserOpen);
        Assert.Contains(client.Invocations, call => string.Equals(call.Method, "browser.close", StringComparison.Ordinal));
    }

    /// <summary>A pasted provider link goes to the daemon's queue, and the status names what arrived without repeating the link.</summary>
    /// <returns>A task representing the test.</returns>
    [Fact]
    public async Task DownloadsHandPastedLinksToTheDaemonWithoutRepeatingThem()
    {
        TestClient client = DownloadQueueClient(() => EmptyQueueJson);
        client.Answer = (method, parameters) => method switch
        {
            "handoff.submit" when parameters.GetProperty("uri").GetString()!.StartsWith("unknown:", StringComparison.Ordinal) =>
                throw new MsbeRpcException("no provider handles unknown:// links"),
            "handoff.submit" => """{"id":7,"provider":"assisted","game":"example","project":"gear","release":"r9","matched":false}""",
            _ => EmptyQueueJson,
        };
        MainViewModel vm = new(client) { IsDownloadQueueSupported = true, HandoffLink = "  handoff://game/files/gear/r9?key=secret  " };

        await vm.SubmitHandoffLinkCommand.ExecuteAsync(parameter: null);

        Assert.Equal("handoff://game/files/gear/r9?key=secret", client.LastParameters("handoff.submit").GetProperty("uri").GetString());
        Assert.Empty(vm.HandoffLink);
        Assert.Equal("Received assisted:gear release r9. Add it to a profile from Downloads.", vm.StatusMessage);
        Assert.Contains(client.Invocations, call => string.Equals(call.Method, "download.list", StringComparison.Ordinal));

        vm.HandoffLink = "unknown://item?key=secret";
        await vm.SubmitHandoffLinkCommand.ExecuteAsync(parameter: null);

        Assert.Equal("unknown://item?key=secret", vm.HandoffLink);
        Assert.Equal("Could not add the link: no provider handles unknown:// links", vm.StatusMessage);
    }

    /// <summary>A link the operating system opens MSBE with before the daemon connects is handed over once it does.</summary>
    /// <returns>A task representing the test.</returns>
    [Fact]
    public async Task LinksArrivingBeforeTheDaemonConnectsAreHandedOverOnceItDoes()
    {
        var client = new TestClient(_ => new CommandResult(0, "[]", string.Empty))
        {
            RpcVersion = 5,
            Answer = (method, _) => method switch
            {
                "handoff.submit" => """{"id":3,"provider":"assisted","game":"example","project":"sprocket","release":"r1","matched":true}""",
                "download.list" => EmptyQueueJson,
                _ => null,
            },
        };
        MainViewModel vm = new(client);
        const string Link = "handoff://game/files/sprocket/r1?key=secret&expires=9";

        await vm.ReceiveLinkAsync(new Uri(Link));
        Assert.DoesNotContain(client.Invocations, call => string.Equals(call.Method, "handoff.submit", StringComparison.Ordinal));

        await vm.ConnectAsync();

        Assert.Equal(Link, client.LastParameters("handoff.submit").GetProperty("uri").GetString());
        Assert.Equal("Received assisted:sprocket release r1; its download continues.", vm.StatusMessage);
    }

    /// <summary>Profile creation can clone the current profile and selects the result.</summary>
    /// <returns>A task representing the test.</returns>
    [Fact]
    public async Task CreateProfileClonesSelectedProfile()
    {
        const string StatusJson = """{"name":"alpha","root":"/games/alpha","plan_id":"minecraft","plan_version":"1","loader":"fabric","game_version":"1.21.1","deployed_profile":"default","deployed_files":2}""";
        const string ProfilesJson = """{"profiles":["default","testing"],"deployed":"default"}""";
        const string ModsJson = """{"order":[],"components":{},"mods":{}}""";
        List<IReadOnlyList<string>> calls = [];
        var client = new TestClient(arguments =>
        {
            calls.Add(arguments);
            if (arguments.Contains("status", StringComparer.Ordinal))
            {
                return new CommandResult(0, StatusJson, string.Empty);
            }

            if (arguments.Contains("list", StringComparer.Ordinal))
            {
                return new CommandResult(0, ProfilesJson, string.Empty);
            }

            return arguments.Contains("new", StringComparer.Ordinal)
                ? new CommandResult(0, "\"testing\"", string.Empty)
                : new CommandResult(0, ModsJson, string.Empty);
        });
        MainViewModel vm = new(client) { SelectedInstance = "alpha" };
        vm.NewProfileName = "testing";
        vm.NewProfileSource = "default";

        await vm.CreateProfileCommand.ExecuteAsync(parameter: null);

        Assert.Contains(calls, arguments => arguments.SequenceEqual(
            ["--format", "json", "profile", "new", "alpha", "testing", "--from", "default"],
            StringComparer.Ordinal));
        Assert.Equal("testing", vm.SelectedProfile);
        Assert.False(vm.HasProfileMutationError);
    }

    /// <summary>Profile compatibility changes invalidate search results and use the structured target command.</summary>
    /// <returns>A task representing the test.</returns>
    [Fact]
    public async Task SaveProfileTargetRefreshesCompatibilityContext()
    {
        const string StatusJson = """{"name":"alpha","root":"/games/alpha","plan_id":"minecraft","plan_version":"1","loader":"fabric","game_version":"1.21.1","deployed_profile":"default","deployed_files":2}""";
        const string ProfilesJson = """{"profiles":["default"],"deployed":"default"}""";
        const string ProfileJson = """{"target":{"loader":"neoforge","loader_version":"21.1.200","side":"server"},"order":[],"components":{},"mods":{}}""";
        List<IReadOnlyList<string>> calls = [];
        var client = new TestClient(arguments =>
        {
            calls.Add(arguments);
            if (arguments.Contains("status", StringComparer.Ordinal))
            {
                return new CommandResult(0, StatusJson, string.Empty);
            }

            if (arguments.Contains("list", StringComparer.Ordinal))
            {
                return new CommandResult(0, ProfilesJson, string.Empty);
            }

            return arguments.Contains("set-target", StringComparer.Ordinal)
                ? new CommandResult(0, "{\"loader\":\"neoforge\",\"loader_version\":\"21.1.200\",\"side\":\"server\"}", string.Empty)
                : new CommandResult(0, ProfileJson, string.Empty);
        });
        MainViewModel vm = new(client) { SelectedInstance = "alpha" };
        var browseResult = new BrowseResultItem("modrinth", "LNytGWDc", "create", "Create", "Aesthetic technology", null, 1, false) { IsMarked = true };
        vm.BrowseResults.Add(browseResult);
        vm.SelectedProfileLoader = "neoforge";
        vm.SelectedProfileLoaderVersion = "21.1.200";
        vm.SelectedProfileSide = "Server";

        await vm.SaveProfileTargetCommand.ExecuteAsync(parameter: null);

        Assert.Contains(calls, arguments => arguments.SequenceEqual(
            ["--format", "json", "profile", "set-target", "alpha", "default", "--loader", "neoforge", "--side", "server", "--loader-version", "21.1.200"],
            StringComparer.Ordinal));
        Assert.Empty(vm.BrowseResults);
        Assert.Empty(vm.MarkedBrowseResults);
        Assert.Equal("neoforge 21.1.200 · Server", vm.SelectedProfileTargetSummary);
        Assert.Equal("Updated compatibility for default.", vm.StatusMessage);
    }

    /// <summary>Source add, selected removal, and rollback use their structured daemon commands.</summary>
    /// <returns>A task representing the test.</returns>
    [Fact]
    public async Task ModMutationsAndRollbackRefreshState()
    {
        const string StatusJson = """{"name":"alpha","root":"/games/alpha","plan_id":"minecraft","plan_version":"1","loader":"fabric","game_version":"1.21.1","deployed_profile":"default","deployed_files":2}""";
        const string ProfilesJson = """{"profiles":["default"],"deployed":"default"}""";
        const string ModsJson = """{"order":["iris"],"components":{},"mods":{"iris":{"origin":"iris.jar","files":[]}}}""";
        List<IReadOnlyList<string>> calls = [];
        var client = new TestClient(arguments =>
        {
            calls.Add(arguments);
            if (arguments.Contains("status", StringComparer.Ordinal))
            {
                return new CommandResult(0, StatusJson, string.Empty);
            }

            if (arguments.Contains("list", StringComparer.Ordinal))
            {
                return new CommandResult(0, ProfilesJson, string.Empty);
            }

            if (arguments.Contains("rollback", StringComparer.Ordinal))
            {
                return new CommandResult(0, "{\"rolled_back\":\"txn-1\"}", string.Empty);
            }

            if (arguments.Contains("add", StringComparer.Ordinal))
            {
                return new CommandResult(0, "{\"added\":[\"iris\"]}", string.Empty);
            }

            if (arguments.Contains("remove", StringComparer.Ordinal))
            {
                return new CommandResult(0, "\"iris\"", string.Empty);
            }

            return new CommandResult(0, ModsJson, string.Empty);
        });
        MainViewModel vm = new(client) { SelectedInstance = "alpha", ModSource = "/mods/iris.jar" };

        await vm.AddModSourceCommand.ExecuteAsync(parameter: null);
        vm.SelectedMod = Assert.Single(vm.Mods);
        await vm.RemoveSelectedModCommand.ExecuteAsync(parameter: null);
        await vm.RollbackLatestCommand.ExecuteAsync(parameter: null);

        Assert.Contains(calls, arguments => arguments.SequenceEqual(
            ["--format", "json", "add", "alpha", "/mods/iris.jar", "--profile", "default", "--with-deps"],
            StringComparer.Ordinal));
        Assert.Contains(calls, arguments => arguments.SequenceEqual(
            ["--format", "json", "remove", "alpha", "iris", "--profile", "default"],
            StringComparer.Ordinal));
        Assert.Contains(calls, arguments => arguments.SequenceEqual(
            ["--format", "json", "rollback", "alpha"],
            StringComparer.Ordinal));
        Assert.Equal("Rolled back the latest deployment.", vm.StatusMessage);
    }

    /// <summary>Pack config editing and validation use structured daemon commands and keep the lockfile path.</summary>
    /// <returns>A task representing the test.</returns>
    [Fact]
    public async Task PackWorkflowEditsAndValidates()
    {
        List<IReadOnlyList<string>> calls = [];
        var client = new TestClient(arguments => PackWorkflowResponse(arguments, calls));
        MainViewModel vm = new(client) { SelectedInstance = "alpha" };
        vm.NewPackConfigCommand.Execute(parameter: null);
        vm.PackConfigPath = "config/example.toml";
        vm.PackConfigContent = "enabled = true\n";

        await vm.SavePackConfigCommand.ExecuteAsync(parameter: null);
        await vm.ValidatePackCommand.ExecuteAsync(parameter: null);

        Assert.Contains(calls, arguments => arguments.SequenceEqual(
            ["--format", "json", "pack", "config", "set", "alpha", "config/example.toml", "--content", "enabled = true\n", "--profile", "default"],
            StringComparer.Ordinal));
        Assert.Contains(calls, arguments => arguments.SequenceEqual(
            ["--format", "json", "pack", "validate", "alpha", "--profile", "default"],
            StringComparer.Ordinal));
        Assert.Equal("/home/test/locks/default.toml", vm.PackLockfilePath);
        Assert.Contains("Pack is valid", vm.StatusMessage, StringComparison.Ordinal);
    }

    /// <summary>Settings opens the data folder the daemon reports, once it has reported one.</summary>
    /// <returns>A task representing the test.</returns>
    [Fact]
    public async Task OpenDataDirectoryOpensTheFolderReportedByTheDaemon()
    {
        var folders = new TestFolderLauncher(succeeds: true);
        MainViewModel vm = new(new TestClient(_ => new CommandResult(0, "[]", string.Empty)), folders);
        Assert.False(vm.OpenDataDirectoryCommand.CanExecute(parameter: null));

        await vm.ConnectAsync();
        await vm.OpenDataDirectoryCommand.ExecuteAsync(parameter: null);

        Assert.Equal(DataDirectory, vm.DataDirectoryText);
        Assert.Equal([DataDirectory], folders.Opened);
        Assert.Equal($"Opened data folder {DataDirectory}.", vm.StatusMessage);
    }

    /// <summary>A data folder the platform cannot open is reported, not silently ignored.</summary>
    /// <returns>A task representing the test.</returns>
    [Fact]
    public async Task OpenDataDirectoryReportsAFolderThatCannotBeOpened()
    {
        MainViewModel vm = new(new TestClient(_ => new CommandResult(0, "[]", string.Empty)), new TestFolderLauncher(succeeds: false));

        await vm.ConnectAsync();
        await vm.OpenDataDirectoryCommand.ExecuteAsync(parameter: null);

        Assert.StartsWith($"Could not open {DataDirectory}.", vm.StatusMessage, StringComparison.Ordinal);
    }

    /// <summary>Settings lists installed extensions with their status, and reports a daemon that cannot list them.</summary>
    /// <returns>A task representing the test.</returns>
    [Fact]
    public async Task SettingsListsInstalledExtensionsWithTheirStatus()
    {
        const string Extensions = """[{"kind":"program","path":"/msbe/extensions/providers/catalog.toml","id":"catalog","version":"0.1.0","signer":"publisher","digest":"aa","status":"active"},{"kind":"codec","path":"/msbe/extensions/codecs/broken.toml","status":"refused","reason":"extension signer \"stranger\" is not trusted"}]""";
        var client = new TestClient(
            _ => new CommandResult(0, "[]", string.Empty),
            method => method switch
            {
                "extension.list" => Extensions,
                _ => throw new InvalidOperationException(method),
            });
        MainViewModel vm = new(client) { IsTypedPackSupported = true };

        await vm.LoadExtensionsCommand.ExecuteAsync(parameter: null);

        Assert.Equal(["catalog 0.1.0", "broken.toml"], vm.InstalledExtensions.Select(extension => extension.Title));
        Assert.Equal(["Active", "Refused"], vm.InstalledExtensions.Select(extension => extension.Status));
        Assert.Equal(["Provider program", "Pack codec"], vm.InstalledExtensions.Select(extension => extension.Kind));
        Assert.Contains("not trusted", vm.InstalledExtensions[1].Detail, StringComparison.Ordinal);
        Assert.Equal("1 of 2 installed extensions are active.", vm.ExtensionsStatus);

        var older = new TestClient(
            _ => new CommandResult(0, "[]", string.Empty),
            _ => throw new MsbeRpcException("method not found", -32601, failureCode: null));
        MainViewModel outdated = new(older) { IsTypedPackSupported = true };
        await outdated.LoadExtensionsCommand.ExecuteAsync(parameter: null);

        Assert.Empty(outdated.InstalledExtensions);
        Assert.StartsWith("This daemon cannot list installed extensions", outdated.ExtensionsStatus, StringComparison.Ordinal);
    }

    /// <summary>Export discovers codecs, renders their schema, previews policy, and runs only the held plan as a job.</summary>
    /// <returns>A task representing the test.</returns>
    [Fact]
    public async Task PackExportPreviewsPolicyAndRunsTheHeldPlanAsAJob()
    {
        const string Codecs = """[{"id":"modrinth-mrpack","provider":"modrinth","name":"Modrinth modpack","extensions":["mrpack"],"directions":{"import":true,"export":true}},{"id":"msbe-native","provider":null,"name":"MSBE native bundle","extensions":["msbepack"],"directions":{"import":true,"export":true}}]""";
        const string Options = """{"codec":"msbe-native","name":"MSBE native bundle","direction":"export","preset":null,"schema":{"schema":1,"presets":[{"id":"portable","name":"Portable","values":{}},{"id":"public-distribution","name":"Public distribution","values":{}}],"fields":[{"key":"purpose","label":"Purpose","description":"Redistribution policy.","required":true,"default":{"type":"choice","value":"private-transfer"},"kind":{"kind":"choice","values":[{"value":"distribute","label":"Distribute publicly"},{"value":"private-transfer","label":"Private transfer"}]}},{"key":"deterministic","label":"Deterministic","description":"Normalized metadata.","required":true,"default":{"type":"boolean","value":true},"kind":{"kind":"boolean"}}]},"values":{"deterministic":true,"purpose":"private-transfer"}}""";
        const string Blocked = """{"plan_id":"plan-1","plan_digest":"sha256:aa","plan":{"items":[{"path":"config/a.toml","group":"embedded-config"},{"path":"mods/local.jar","group":"policy-blocker"}],"requirements":[],"environment":[],"blockers":[{"code":"DistributionUnknown","message":"mods/local.jar has no exact source"}],"warnings":[],"observations":[{"subject":"sha256:01","observed_at":"2026-09-10"}],"embedded_bytes":12}}""";
        const string Clean = """{"plan_id":"plan-2","plan_digest":"sha256:bb","plan":{"items":[{"path":"config/a.toml","group":"embedded-config"}],"requirements":[],"environment":[],"blockers":[],"warnings":[],"observations":[],"embedded_bytes":12}}""";
        var previews = new Queue<string>([Blocked, Clean]);
        List<IReadOnlyList<string>> calls = [];
        var client = new TestClient(
            arguments => PackWorkflowResponse(arguments, calls),
            method => method switch
            {
                "pack.codec.list" => Codecs,
                "pack.codec.options" => Options,
                "pack.export.preview" => previews.Dequeue(),
                "job.start" => """{"job_id":7}""",
                "job.events" => """{"job_id":7,"method":"pack.export.execute","state":"succeeded","events":[{"sequence":1,"kind":"done","result":{}}],"next":1}""",
                _ => throw new InvalidOperationException(method),
            });
        MainViewModel vm = new(client, folders: null, new FixedTime(new DateTimeOffset(2026, 9, 12, 8, 0, 0, TimeSpan.Zero)))
        {
            SelectedInstance = "alpha",
            IsTypedPackSupported = true,
        };

        await vm.LoadExportCodecsCommand.ExecuteAsync(parameter: null);

        Assert.Equal("msbe-native", vm.SelectedExportCodec?.Id);
        Assert.EndsWith("alpha-default.msbepack", vm.PackOutputPath, StringComparison.Ordinal);
        Assert.Equal(["purpose", "deterministic"], vm.ExportOptions.Select(option => option.Key));
        Assert.Equal("private-transfer", vm.ExportOptions[0].SelectedChoice?.Value);
        Assert.True(vm.ExportOptions[1].BooleanValue);
        Assert.Equal(["portable", "public-distribution"], vm.ExportPresets.Select(preset => preset.Id));

        await vm.PreviewExportCommand.ExecuteAsync(parameter: null);

        Assert.False(vm.CanExecuteExport);
        Assert.Equal(["Embedded config", "Policy blocker"], vm.ExportPreviewItems.Select(item => item.Group));
        Assert.Equal("DistributionUnknown", Assert.Single(vm.ExportIssues).Code);
        Assert.Equal("sha256:01 observed 2 day(s) ago", Assert.Single(vm.ExportObservations));
        JsonElement previewed = client.LastParameters("pack.export.preview");
        Assert.Equal("private-transfer", previewed.GetProperty("options").GetProperty("purpose").GetString());
        Assert.True(previewed.GetProperty("options").GetProperty("deterministic").GetBoolean());

        await vm.PreviewExportCommand.ExecuteAsync(parameter: null);
        Assert.True(vm.CanExecuteExport);
        await vm.ExecuteExportCommand.ExecuteAsync(parameter: null);

        JsonElement started = client.LastParameters("job.start");
        Assert.Equal("pack.export.execute", started.GetProperty("method").GetString());
        Assert.Equal("plan-2", started.GetProperty("params").GetProperty("plan_id").GetString());
        Assert.Equal("sha256:bb", started.GetProperty("params").GetProperty("plan_digest").GetString());
        Assert.StartsWith("Exported default to", vm.StatusMessage, StringComparison.Ordinal);
        Assert.False(vm.HasExportPreview);
        Assert.False(vm.IsPackJobRunning);
    }

    /// <summary>Update previews list layer conflicts, send the chosen resolutions, and run the resolved plan.</summary>
    /// <returns>A task representing the test.</returns>
    [Fact]
    public async Task PackUpdateResolvesLayerConflictsBeforeRunning()
    {
        const string Conflicted = """{"plan_id":"plan-1","plan_digest":"sha256:aa","plan":{"items":[{"subject":"a","action":"reuse"}],"changes":[{"kind":"add-mod","subject":"c"}],"conflicts":[{"id":"mod:c","change":{"kind":"add-mod","subject":"c"},"reason":"the updated pack now ships a mod with this name"}],"blockers":[{"code":"LayerConflict","message":"mod:c: resolve it as keep or drop"}],"warnings":[]}}""";
        const string Resolved = """{"plan_id":"plan-2","plan_digest":"sha256:bb","plan":{"items":[{"subject":"a","action":"reuse"}],"changes":[{"kind":"add-mod","subject":"c"}],"conflicts":[{"id":"mod:c","change":{"kind":"add-mod","subject":"c"},"reason":"the updated pack now ships a mod with this name","resolution":"keep"}],"blockers":[],"warnings":[]}}""";
        var previews = new Queue<string>([Conflicted, Resolved]);
        List<IReadOnlyList<string>> calls = [];
        var client = new TestClient(
            arguments => PackWorkflowResponse(arguments, calls),
            method => method switch
            {
                "pack.update.preview" => previews.Dequeue(),
                "job.start" => """{"job_id":3}""",
                "job.events" => """{"job_id":3,"method":"pack.update.execute","state":"succeeded","events":[{"sequence":1,"kind":"done","result":{}}],"next":1}""",
                _ => throw new InvalidOperationException(method),
            });
        MainViewModel vm = new(client) { SelectedInstance = "alpha", PackImportPath = "/packs/v2.mrpack" };

        await vm.PreviewUpdateCommand.ExecuteAsync(parameter: null);

        Assert.False(vm.CanExecuteImport);
        PackConflictItem conflict = Assert.Single(vm.ImportConflicts);
        Assert.Equal("mod:c", conflict.Id);
        Assert.Contains(vm.ImportPreviewItems, item => string.Equals(item.Subject, "add-mod c", StringComparison.Ordinal));

        conflict.Resolution = "keep";
        await vm.PreviewUpdateCommand.ExecuteAsync(parameter: null);

        Assert.Equal("keep", client.LastParameters("pack.update.preview").GetProperty("resolutions").GetProperty("mod:c").GetString());
        Assert.True(vm.CanExecuteImport);
        await vm.ExecuteImportCommand.ExecuteAsync(parameter: null);

        Assert.Equal("pack.update.execute", client.LastParameters("job.start").GetProperty("method").GetString());
        Assert.Equal("Updated the pack layer of default.", vm.StatusMessage);
    }

    /// <summary>Accounts accepts terms, sends a pasted key once without keeping or repeating it, and signs out.</summary>
    /// <returns>A task representing the test.</returns>
    [Fact]
    public async Task AccountsAcceptTermsSignInWithAPastedKeyAndSignOut()
    {
        const string Key = "pasted-provider-key-0123";
        const string KeyPage = "https://www.keyed.test/account/keys";
        const string Unsigned = """{"provider":"keyed","name":"Keyed","requires_auth":true,"signed_in":false,"key_page":"https://www.keyed.test/account/keys","terms":"https://www.keyed.test/terms","ack_required":true,"acknowledged":false}""";
        const string Accepted = """{"provider":"keyed","name":"Keyed","requires_auth":true,"signed_in":false,"key_page":"https://www.keyed.test/account/keys","terms":"https://www.keyed.test/terms","ack_required":true,"acknowledged":true}""";
        const string SignedIn = """{"provider":"keyed","name":"Keyed","requires_auth":true,"signed_in":true,"source":"keyring","account":"Player","key_page":"https://www.keyed.test/account/keys","terms":"https://www.keyed.test/terms","ack_required":true,"acknowledged":true,"quota":{"x-hourly-remaining":90}}""";
        var links = new TestLinkLauncher();
        var client = new TestClient(_ => new CommandResult(0, "[]", string.Empty))
        {
            Answer = (method, _) => method switch
            {
                "auth.status" => $"[{Unsigned}]",
                "auth.acknowledge" or "auth.logout" => Accepted,
                "auth.login" => SignedIn,
                "provider.list" => "[]",
                _ => null,
            },
        };
        MainViewModel vm = new(client, folders: null, time: null, links);

        await vm.LoadAccountsCommand.ExecuteAsync(parameter: null);
        AccountItem account = Assert.Single(vm.Accounts);
        Assert.Equal(Strings.AccountSignInRequired, account.SignInText);
        Assert.Equal(Strings.FormatAccountsSummary(0, 1), vm.AccountsStatus);
        account.Key = Key;
        Assert.False(account.CanSignIn);

        await vm.AcknowledgeTermsCommand.ExecuteAsync(account);
        Assert.True(account.CanSignIn);
        await vm.SignInCommand.ExecuteAsync(account);

        Assert.Equal(Key, client.LastParameters("auth.login").GetProperty("token").GetString());
        Assert.Empty(account.Key);
        Assert.True(account.IsSignedIn);
        Assert.Equal(Strings.FormatAccountSignedInAs("Player", Strings.CredentialSourceKeyring), account.SignInText);
        Assert.Equal(Strings.FormatAccountQuota(90, "x-hourly-remaining"), account.QuotaText);
        Assert.Equal(Strings.FormatAccountSignedInStatus("Keyed"), vm.StatusMessage);

        await vm.OpenWebPageCommand.ExecuteAsync(account.KeyPage);
        Assert.Equal([new Uri(KeyPage)], links.Opened);

        await vm.SignOutCommand.ExecuteAsync(account);
        Assert.False(account.IsSignedIn);
        Assert.Equal("keyed", client.LastParameters("auth.logout").GetProperty("provider").GetString());
    }

    /// <summary>Registering MSBE for a scheme another application opens asks first, and replaces it only once confirmed.</summary>
    /// <returns>A task representing the test.</returns>
    [Fact]
    public async Task LinkHandlersAskBeforeTakingASchemeFromAnotherApplication()
    {
        const string Owner = "Other (other.desktop)";
        const string Other = """{"scheme":"handoff","provider":"assisted","owner":{"kind":"other","name":"Other (other.desktop)"},"current":false}""";
        const string Registered = """{"scheme":"handoff","provider":"assisted","owner":{"kind":"msbe"},"current":true,"previous":"Other (other.desktop)"}""";
        var client = new TestClient(_ => new CommandResult(0, "[]", string.Empty))
        {
            Answer = (method, parameters) => method switch
            {
                "handler.status" => $"[{Other}]",
                "handler.register" when !parameters.GetProperty("replace").GetBoolean() =>
                    throw new MsbeRpcException("another application opens handoff links", HandlerRpc.OwnedByAnotherApplication, failureCode: null),
                "handler.register" => Registered,
                "handler.unregister" => Other,
                _ => null,
            },
        };
        MainViewModel vm = new(client);

        await vm.LoadLinkHandlersCommand.ExecuteAsync(parameter: null);
        HandlerItem handler = Assert.Single(vm.LinkHandlers);
        Assert.Equal(Strings.FormatHandlerOwnedByOther(Owner), handler.OwnerText);
        Assert.True(handler.CanRegister);

        await vm.RegisterLinkHandlerCommand.ExecuteAsync(handler);
        Assert.True(handler.IsConfirmingReplace);
        Assert.False(handler.IsRegistered);
        Assert.Equal(Strings.FormatHandlerReplaceConfirm(Owner, "handoff"), handler.ReplaceText);

        await vm.ConfirmLinkHandlerReplaceCommand.ExecuteAsync(handler);
        Assert.True(client.LastParameters("handler.register").GetProperty("replace").GetBoolean());
        Assert.True(handler.IsRegistered);
        Assert.False(handler.IsConfirmingReplace);
        Assert.Equal(Strings.FormatHandlerOwnedByMsbeReplacing(Owner), handler.OwnerText);

        await vm.UnregisterLinkHandlerCommand.ExecuteAsync(handler);
        Assert.False(handler.IsOwnedByMsbe);
        Assert.Equal(Strings.FormatHandlerUnregisteredStatus("handoff"), vm.StatusMessage);
    }

    /// <summary>External tools register only an absolute program whose provider's terms are accepted, and forget it.</summary>
    /// <returns>A task representing the test.</returns>
    [Fact]
    public async Task ExternalToolsRegisterAnAbsoluteProgramOnlyOnceTheTermsAreAccepted()
    {
        const string Unregistered = """{"provider":"example-tool","name":"Example tool","terms":"https://www.example.test/terms","state":"unregistered"}""";
        const string Registered = """{"provider":"example-tool","name":"Example tool","terms":"https://www.example.test/terms","program":"/opt/tool/fetch","sha256":"0123456789abcdef0123","state":"registered"}""";
        string program = OperatingSystem.IsWindows() ? @"C:\tools\fetch.exe" : "/opt/tool/fetch";
        var client = new TestClient(_ => new CommandResult(0, "[]", string.Empty))
        {
            Answer = (method, _) => method switch
            {
                "tool.list" => $"[{Unregistered}]",
                "tool.register" => Registered,
                "tool.forget" => Unregistered,
                _ => null,
            },
        };
        MainViewModel vm = new(client);

        await vm.LoadExternalToolsCommand.ExecuteAsync(parameter: null);
        ToolItem tool = Assert.Single(vm.ExternalTools);
        Assert.Equal(Strings.ToolUnregistered, tool.StateText);

        tool.ProgramPath = "relative/fetch";
        tool.AcceptsTerms = true;
        Assert.False(tool.CanRegister);
        tool.ProgramPath = program;
        Assert.True(tool.CanRegister);
        await vm.RegisterExternalToolCommand.ExecuteAsync(tool);

        JsonElement registered = client.LastParameters("tool.register");
        Assert.Equal(program, registered.GetProperty("program").GetString());
        Assert.True(registered.GetProperty("accept_terms").GetBoolean());
        Assert.Equal(Strings.ToolRegistered, tool.StateText);
        Assert.Equal(Strings.FormatToolProgram("/opt/tool/fetch", "0123456789ab"), tool.ProgramText);
        Assert.False(tool.AcceptsTerms);

        await vm.ForgetExternalToolCommand.ExecuteAsync(tool);
        Assert.False(tool.HasProgram);
        Assert.Equal(Strings.FormatToolForgottenStatus("Example tool"), vm.StatusMessage);
    }

    /// <summary>Without the browser component, a waiting download's page opens in the user's own browser, and only over HTTPS.</summary>
    /// <returns>A task representing the test.</returns>
    [Fact]
    public async Task WaitingPagesOpenInTheUsersBrowserWhenTheBrowserComponentIsMissing()
    {
        const string Missing = """{"running":false,"installed":false,"waiting":1,"auto_advance":false}""";
        const string ToolPage = "https://www.example.test/tool";
        var links = new TestLinkLauncher();
        TestClient client = DownloadQueueClient(() => QueueJson);
        client.Answer = (method, _) => method switch
        {
            "download.list" => QueueJson,
            "browser.status" => Missing,
            _ => EmptyQueueJson,
        };
        MainViewModel vm = new(client, folders: null, time: null, links) { IsDownloadQueueSupported = true };

        await vm.RefreshDownloadsCommand.ExecuteAsync(parameter: null);

        Assert.False(vm.IsBrowserComponentInstalled);
        Assert.False(vm.CanOpenWaitingPages);
        Assert.Equal(Strings.BrowserComponentMissing, vm.BrowserComponentStatus);
        await vm.OpenDownloadPageInBrowserCommand.ExecuteAsync(vm.QueuedDownloads[1]);
        Assert.Equal([new Uri(ToolPage)], links.Opened);
        Assert.Equal(Strings.FormatWebPageOpened("www.example.test"), vm.StatusMessage);

        await vm.OpenWebPageCommand.ExecuteAsync("http://www.example.test/plain");
        Assert.Equal(Strings.WebPageNotHttps, vm.StatusMessage);
        Assert.Single(links.Opened);
    }

    /// <summary>Browse says which results wait for the user before they are queued, and adds projects from providers without search by reference.</summary>
    /// <returns>A task representing the test.</returns>
    [Fact]
    public async Task BrowseSaysWhichResultsNeedTheUserAndAddsFromProvidersWithoutSearch()
    {
        const string StatusJson = """{"name":"alpha","root":"/games/alpha","plan_id":"minecraft","plan_version":"1","loader":"fabric","game_version":"1.21.1","deployed_profile":"default","deployed_files":0}""";
        const string ProfilesJson = """{"profiles":["default"],"deployed":"default"}""";
        const string ModsJson = """{"order":[],"components":{},"mods":{}}""";
        const string SearchJson = """[{"provider":"assisted","project":"gear","slug":"gear","title":"Gear","description":"Cogs","icon_url":null,"downloads":5},{"provider":"modrinth","project":"YL57xq9U","slug":"iris","title":"Iris","description":"Shader support","icon_url":null,"downloads":9000000}]""";
        const string Providers = """[{"id":"assisted","name":"Assisted","prefix":"assisted:","search":true,"acquisition":"browser_assisted","requires_auth":false,"signed_in":false,"ack_required":false,"acknowledged":true},{"id":"linked","name":"Linked","prefix":"linked:","search":false,"acquisition":"direct_https","requires_auth":false,"signed_in":false,"ack_required":false,"acknowledged":true},{"id":"modrinth","name":"Modrinth","prefix":"modrinth:","search":true,"acquisition":"direct_https","requires_auth":false,"signed_in":false,"ack_required":false,"acknowledged":true}]""";
        const string Queued = """{"id":9,"revision":1,"source":"linked:4242","target":{"instance":"alpha","profile":"default"},"with_deps":true,"attempts":0,"state":{"kind":"queued"},"files":[]}""";
        var client = new TestClient(arguments =>
        {
            string output = arguments.FirstOrDefault(argument => argument is "status" or "list" or "search") switch
            {
                "status" => StatusJson,
                "list" => ProfilesJson,
                "search" => SearchJson,
                _ => ModsJson,
            };
            return new CommandResult(0, output, string.Empty);
        })
        {
            Answer = (method, _) => method switch
            {
                "provider.list" => Providers,
                "download.enqueue" => Queued,
                _ => EmptyQueueJson,
            },
        };
        MainViewModel vm = new(client) { SelectedInstance = "alpha", BrowseQuery = "cogs", IsDownloadQueueSupported = true };

        await vm.LoadProvidersCommand.ExecuteAsync(parameter: null);
        await vm.SearchBrowseCommand.ExecuteAsync(parameter: null);

        Assert.Equal(Strings.FormatBrowseNeedsPage("Assisted"), vm.BrowseResults[0].Attention);
        Assert.False(vm.BrowseResults[1].NeedsUser);
        vm.BrowseResults[0].IsMarked = true;
        vm.BrowseResults[1].IsMarked = true;
        Assert.True(vm.HasMarkedNeedingUser);
        Assert.Equal(Strings.FormatBrowseMarkedNeedUser(1), vm.MarkedNeedUserText);

        Assert.Equal("linked", Assert.Single(vm.LinkProviders).Id);
        Assert.Equal("linked", vm.SelectedLinkProvider?.Id);
        Assert.Equal(Strings.FormatBrowseLinkPlaceholder("linked:"), vm.LinkReferencePlaceholder);
        vm.LinkReference = " 4242 ";
        await vm.AddByLinkCommand.ExecuteAsync(parameter: null);

        Assert.Equal("linked:4242", client.LastParameters("download.enqueue").GetProperty("source").GetString());
        Assert.Empty(vm.LinkReference);
        Assert.Equal(Strings.FormatBrowseAddedByLink("linked:4242", "Linked", "default"), vm.StatusMessage);
    }

    /// <summary>History rolls back to a chosen deployment only after confirming how many later deployments it undoes.</summary>
    /// <returns>A task representing the test.</returns>
    [Fact]
    public async Task HistoryRollsBackToAChosenDeploymentOnlyAfterConfirming()
    {
        const string RolledBack = """{"rolled_back":[3,2],"journal":[{"txn":1,"profile":"default","files":3}]}""";
        string journal = """[{"txn":1,"profile":"default","files":3},{"txn":2,"profile":"default","files":4},{"txn":3,"profile":"testing","files":5}]""";
        string RollBack()
        {
            journal = """[{"txn":1,"profile":"default","files":3}]""";
            return RolledBack;
        }

        (TestClient client, _) = HistoryClient(() => journal, RollBack);
        MainViewModel vm = new(client) { SelectedInstance = "alpha" };

        vm.NavigateCommand.Execute(WorkspacePage.History);
        await vm.LoadHistoryCommand.ExecuteAsync(parameter: null);

        Assert.Equal([3L, 2L, 1L], vm.JournalEntries.Select(entry => entry.Transaction));
        Assert.True(vm.JournalEntries[0].IsDeployed);
        Assert.Equal(Strings.FormatJournalDetail("testing", 5), vm.JournalEntries[0].Detail);
        vm.RequestRollbackToCommand.Execute(vm.JournalEntries[0]);
        Assert.False(vm.HasPendingRollback);
        vm.RequestRollbackToCommand.Execute(vm.JournalEntries[2]);
        Assert.Equal(Strings.FormatJournalConfirmRollback(2, 1), vm.PendingRollbackText);
        Assert.DoesNotContain(client.Invocations, call => string.Equals(call.Method, "journal.rollback", StringComparison.Ordinal));

        await vm.ConfirmRollbackCommand.ExecuteAsync(parameter: null);

        JsonElement rollback = client.LastParameters("journal.rollback");
        Assert.Equal("alpha", rollback.GetProperty("instance").GetString());
        Assert.Equal(1L, rollback.GetProperty("txn").GetInt64());
        Assert.Equal([1L], vm.JournalEntries.Select(entry => entry.Transaction));
        Assert.False(vm.HasPendingRollback);
        Assert.Equal(Strings.FormatJournalRolledBack(1, 2), vm.StatusMessage);
    }

    /// <summary>History removes a conflicting mod, and applies updates and restores a snapshot as jobs.</summary>
    /// <returns>A task representing the test.</returns>
    [Fact]
    public async Task HistoryRemovesConflictingModsAndRunsUpdatesAndSnapshotRestoreAsJobs()
    {
        (TestClient client, List<IReadOnlyList<string>> calls) = HistoryClient(
            () => """[{"txn":1,"profile":"default","files":3}]""",
            () => """{"rolled_back":[],"journal":[]}""");
        MainViewModel vm = new(client) { SelectedInstance = "alpha" };
        vm.NavigateCommand.Execute(WorkspacePage.History);

        ConflictItem conflict = Assert.Single(vm.Conflicts);
        Assert.Equal("mods/common.jar", conflict.Path);
        Assert.Equal(["pack-a", "pack-b"], conflict.Claims.Select(claim => claim.Module));
        Assert.Equal(Strings.FormatConflictClaim("sha256:bbbbbbbbbbbb"), conflict.Claims[1].Detail);
        await vm.RemoveConflictingModCommand.ExecuteAsync(conflict.Claims[1]);
        Assert.Contains(calls, arguments => arguments.SequenceEqual(["--format", "json", "remove", "alpha", "pack-b", "--profile", "default"], StringComparer.Ordinal));
        Assert.Equal(Strings.FormatConflictRemovedStatus("pack-b", "default"), vm.StatusMessage);

        await vm.CheckUpdatesCommand.ExecuteAsync(parameter: null);
        Assert.Equal(Strings.FormatUpdateChange("1.7.0", "1.8.0"), Assert.Single(vm.AvailableUpdates).Change);
        Assert.Equal(Strings.FormatUpdatesOther(1, 0, 0, 1), vm.UpdatesDetail);
        Assert.True(vm.CanApplyUpdates);
        await vm.ApplyUpdatesCommand.ExecuteAsync(parameter: null);

        JsonElement started = client.LastParameters("job.start");
        Assert.Equal("update.apply", started.GetProperty("method").GetString());
        Assert.Equal("default", started.GetProperty("params").GetProperty("profile").GetString());
        Assert.Empty(vm.AvailableUpdates);
        Assert.Equal(Strings.FormatUpdatesApplied(1, "default"), vm.StatusMessage);

        string snapshot = Path.Combine(Path.GetTempPath(), "alpha.msbesnapshot");
        vm.SnapshotRestorePath = snapshot;
        vm.RequestSnapshotRestoreCommand.Execute(parameter: null);
        Assert.True(vm.IsConfirmingSnapshotRestore);
        await vm.ConfirmSnapshotRestoreCommand.ExecuteAsync(parameter: null);

        started = client.LastParameters("job.start");
        Assert.Equal("snapshot.restore", started.GetProperty("method").GetString());
        Assert.Equal(snapshot, started.GetProperty("params").GetProperty("input").GetString());
        Assert.Equal(Strings.FormatSnapshotRestored(snapshot), vm.StatusMessage);
    }

    /// <summary>A client whose daemon answers the History workspace's methods, and the commands it ran.</summary>
    private static (TestClient Client, List<IReadOnlyList<string>> Calls) HistoryClient(Func<string> journal, Func<string> rollBack)
    {
        const string StatusJson = """{"name":"alpha","root":"/games/alpha","plan_id":"minecraft","plan_version":"1","loader":"fabric","game_version":"1.21.1","deployed_profile":"default","deployed_files":0}""";
        const string ProfilesJson = """{"profiles":["default"],"deployed":"default"}""";
        const string ModsJson = """{"order":[],"components":{},"mods":{}}""";
        const string Conflicts = """[{"path":"mods/common.jar","claims":[{"module":"pack-a","blob":"sha256:aaaaaaaaaaaaaaaaaaaaaaaa"},{"module":"pack-b","blob":"sha256:bbbbbbbbbbbbbbbbbbbbbbbb"}]}]""";
        const string Updates = """{"dry_run":true,"updated":[{"module":"iris","from":"1.7.0","to":"1.8.0"}],"current":["sodium"],"no_compatible_version":[],"unlisted":[],"not_updatable":["local"],"unresolved":[],"incompatible":[]}""";
        const string JobDone = """{"job_id":5,"method":"update.apply","state":"succeeded","events":[{"sequence":1,"kind":"done","result":{}}],"next":1}""";
        List<IReadOnlyList<string>> calls = [];
        var client = new TestClient(arguments =>
        {
            calls.Add(arguments);
            string output = arguments.FirstOrDefault(argument => argument is "status" or "list" or "remove") switch
            {
                "status" => StatusJson,
                "list" => ProfilesJson,
                "remove" => "\"pack-b\"",
                _ => ModsJson,
            };
            return new CommandResult(0, output, string.Empty);
        })
        {
            Answer = (method, _) => method switch
            {
                "journal.list" => journal(),
                "journal.rollback" => rollBack(),
                "conflicts.list" => Conflicts,
                "update.preview" => Updates,
                "job.start" => """{"job_id":5}""",
                "job.events" => JobDone,
                _ => null,
            },
        };
        return (client, calls);
    }

    private static CommandResult PackWorkflowResponse(IReadOnlyList<string> arguments, List<IReadOnlyList<string>> calls)
    {
        calls.Add(arguments);
        if (arguments.Contains("status", StringComparer.Ordinal))
        {
            return new CommandResult(0, """{"name":"alpha","root":"/games/alpha","plan_id":"minecraft","plan_version":"1","loader":"fabric","game_version":"1.21.1","deployed_profile":"default","deployed_files":0}""", string.Empty);
        }

        if (arguments.SequenceEqual(["--format", "json", "profile", "list", "alpha"], StringComparer.Ordinal))
        {
            return new CommandResult(0, """{"profiles":["default"],"deployed":null}""", string.Empty);
        }

        if (arguments.Contains("config", StringComparer.Ordinal) && arguments.Contains("list", StringComparer.Ordinal))
        {
            return new CommandResult(0, """[{"path":"config/example.toml","digest":"sha256:01"}]""", string.Empty);
        }

        if (arguments.Contains("show", StringComparer.Ordinal) && arguments.Contains("config", StringComparer.Ordinal))
        {
            return new CommandResult(0, """{"path":"config/example.toml","digest":"sha256:01","content":"enabled = true\n"}""", string.Empty);
        }

        if (arguments.Contains("validate", StringComparer.Ordinal))
        {
            return new CommandResult(0, """{"lockfile":"/home/test/locks/default.toml","files":1,"configs":1,"mods":0}""", string.Empty);
        }

        return arguments.Contains("set", StringComparer.Ordinal)
            ? new CommandResult(0, """{"path":"config/example.toml","digest":"sha256:01"}""", string.Empty)
            : new CommandResult(0, """{"target":{"loader":"fabric","loader_version":"0.16.10","side":"client"},"order":[],"components":{},"mods":{},"configs":{}}""", string.Empty);
    }

    /// <summary>A client whose daemon lists <paramref name="queue" /> and accepts every download control.</summary>
    private static TestClient DownloadQueueClient(Func<string> queue)
    {
        const string StatusJson = """{"name":"alpha","root":"/games/alpha","plan_id":"minecraft","plan_version":"1","loader":"fabric","game_version":"1.21.1","deployed_profile":"default","deployed_files":0}""";
        const string ProfilesJson = """{"profiles":["default"],"deployed":"default"}""";
        const string ModsJson = """{"order":[],"components":{},"mods":{}}""";
        return new TestClient(arguments =>
        {
            string output = arguments.FirstOrDefault(argument => argument is "status" or "list") switch
            {
                "status" => StatusJson,
                "list" => ProfilesJson,
                _ => ModsJson,
            };
            return new CommandResult(0, output, string.Empty);
        })
        {
            Answer = (method, _) => method switch
            {
                "download.list" => queue(),
                "download.cancel" or "download.retry" or "download.confirm" => """{"id":2,"revision":9,"attempts":0,"state":{"kind":"queued"},"files":[]}""",
                _ => EmptyQueueJson,
            },
        };
    }

    private sealed class TestLinkLauncher : ILinkLauncher
    {
        public List<Uri> Opened { get; } = [];

        public Task<bool> OpenAsync(Uri page)
        {
            this.Opened.Add(page);
            return Task.FromResult(true);
        }
    }

    private sealed class TestFolderLauncher : IFolderLauncher
    {
        private readonly bool succeeds;

        public TestFolderLauncher(bool succeeds) => this.succeeds = succeeds;

        public List<string> Opened { get; } = [];

        public Task<bool> OpenAsync(string path)
        {
            this.Opened.Add(path);
            return Task.FromResult(this.succeeds);
        }
    }

    private sealed class TestClient : IMsbeClient
    {
        private readonly Func<IReadOnlyList<string>, Task<CommandResult>> runCommand;
        private readonly Func<string, string>? invoke;

        public TestClient(Func<IReadOnlyList<string>, CommandResult> runCommand, Func<string, string>? invoke = null)
            : this(arguments => Task.FromResult(runCommand(arguments)), invoke)
        {
        }

        public TestClient(Func<IReadOnlyList<string>, Task<CommandResult>> runCommand, Func<string, string>? invoke = null)
        {
            this.runCommand = runCommand;
            this.invoke = invoke;
        }

        public List<(string Method, JsonElement Parameters)> Invocations { get; } = [];

        /// <summary>Gets or sets the result JSON for an RPC method and its parameters, before <c>invoke</c> is asked.</summary>
        public Func<string, JsonElement, string?>? Answer { get; set; }

        /// <summary>Gets the RPC contract version the daemon reports.</summary>
        public int RpcVersion { get; init; } = 3;

        public Task<DaemonInfo> GetInfoAsync(CancellationToken cancellationToken) => Task.FromResult(new DaemonInfo("test", this.RpcVersion, DataDirectory));

        public Task<IReadOnlyList<GameInfo>> GetGamesAsync(CancellationToken cancellationToken) => Task.FromResult<IReadOnlyList<GameInfo>>(
            [new GameInfo("minecraft", "Minecraft", "1", ["fabric", "neoforge"])]);

        public Task<CommandResult> RunCommandAsync(IReadOnlyList<string> arguments, CancellationToken cancellationToken) => this.runCommand(arguments);

        public Task<JsonElement> InvokeAsync(string method, Action<Utf8JsonWriter>? writeParameters, CancellationToken cancellationToken)
        {
            var buffer = new ArrayBufferWriter<byte>();
#pragma warning disable MA0042, MA0045 // Utf8JsonWriter writes to memory synchronously; there is nothing to await.
            using (var writer = new Utf8JsonWriter(buffer))
            {
                if (writeParameters is null)
                {
                    writer.WriteNullValue();
                }
                else
                {
                    writeParameters(writer);
                }
            }
#pragma warning restore MA0042, MA0045

            using JsonDocument parameters = JsonDocument.Parse(buffer.WrittenMemory);
            this.Invocations.Add((method, parameters.RootElement.Clone()));
            string result = this.Answer?.Invoke(method, parameters.RootElement) ?? this.invoke?.Invoke(method) ?? throw new InvalidOperationException($"Unexpected RPC {method}.");
            using JsonDocument document = JsonDocument.Parse(result);
            return Task.FromResult(document.RootElement.Clone());
        }

        public JsonElement LastParameters(string method) =>
            this.Invocations.Last(call => string.Equals(call.Method, method, StringComparison.Ordinal)).Parameters;
    }

    private sealed class FixedTime : TimeProvider
    {
        private readonly DateTimeOffset now;

        public FixedTime(DateTimeOffset now) => this.now = now;

        public override DateTimeOffset GetUtcNow() => this.now;
    }
}
