using MSBE.Client;
using MSBE.Desktop.Services;
using MSBE.Desktop.ViewModels;

using Xunit;

namespace MSBE.Desktop.Tests;

/// <summary>Tests for <see cref="MainViewModel" />.</summary>
public sealed class MainViewModelTests
{
    private const string DataDirectory = "/data/msbe";

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

            if (arguments.Contains("add", StringComparer.Ordinal))
            {
                return new CommandResult(0, "{\"added\":[\"sodium\"],\"skipped\":[],\"unresolved\":[],\"incompatible\":[],\"substituted\":[]}", string.Empty);
            }

            return new CommandResult(0, ModsJson, string.Empty);
        });
        MainViewModel vm = new(client) { SelectedInstance = "alpha", BrowseQuery = "rendering" };

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
        Assert.Contains(calls, arguments => arguments.SequenceEqual(
            ["--format", "json", "add", "alpha", "modrinth:iris", "--profile", "default", "--with-deps"],
            StringComparer.Ordinal));
        Assert.Contains("Added 1 mod(s)", vm.StatusMessage, StringComparison.Ordinal);
        Assert.Empty(vm.MarkedBrowseResults);
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

    /// <summary>Pack config editing, validation, and export use structured daemon commands.</summary>
    /// <returns>A task representing the test.</returns>
    [Fact]
    public async Task PackWorkflowEditsValidatesAndExports()
    {
        List<IReadOnlyList<string>> calls = [];
        var client = new TestClient(arguments => PackWorkflowResponse(arguments, calls));
        MainViewModel vm = new(client) { SelectedInstance = "alpha" };
        vm.NewPackConfigCommand.Execute(parameter: null);
        vm.PackConfigPath = "config/example.toml";
        vm.PackConfigContent = "enabled = true\n";

        await vm.SavePackConfigCommand.ExecuteAsync(parameter: null);
        await vm.ValidatePackCommand.ExecuteAsync(parameter: null);
        vm.PackOutputPath = "/home/test/alpha.mrpack";
        await vm.ExportPackCommand.ExecuteAsync(parameter: null);

        Assert.Contains(calls, arguments => arguments.SequenceEqual(
            ["--format", "json", "pack", "config", "set", "alpha", "config/example.toml", "--content", "enabled = true\n", "--profile", "default"],
            StringComparer.Ordinal));
        Assert.Contains(calls, arguments => arguments.SequenceEqual(
            ["--format", "json", "pack", "validate", "alpha", "--profile", "default"],
            StringComparer.Ordinal));
        Assert.Contains(calls, arguments => arguments.SequenceEqual(
            ["--format", "json", "pack", "export", "alpha", "--output", "/home/test/alpha.mrpack", "--profile", "default"],
            StringComparer.Ordinal));
        Assert.Equal("/home/test/locks/default.toml", vm.PackLockfilePath);
        Assert.Contains("Exported distributable pack", vm.StatusMessage, StringComparison.Ordinal);
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

        if (arguments.Contains("export", StringComparer.Ordinal))
        {
            return new CommandResult(0, """{"output":"/home/test/alpha.mrpack","files":1}""", string.Empty);
        }

        return arguments.Contains("set", StringComparer.Ordinal)
            ? new CommandResult(0, """{"path":"config/example.toml","digest":"sha256:01"}""", string.Empty)
            : new CommandResult(0, """{"target":{"loader":"fabric","loader_version":"0.16.10","side":"client"},"order":[],"components":{},"mods":{},"configs":{}}""", string.Empty);
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
        private readonly Func<IReadOnlyList<string>, CommandResult> runCommand;

        public TestClient(Func<IReadOnlyList<string>, CommandResult> runCommand) => this.runCommand = runCommand;

        public Task<DaemonInfo> GetInfoAsync(CancellationToken cancellationToken) => Task.FromResult(new DaemonInfo("test", 3, DataDirectory));

        public Task<IReadOnlyList<GameInfo>> GetGamesAsync(CancellationToken cancellationToken) => Task.FromResult<IReadOnlyList<GameInfo>>(
            [new GameInfo("minecraft", "Minecraft", "1", ["fabric", "neoforge"])]);

        public Task<CommandResult> RunCommandAsync(IReadOnlyList<string> arguments, CancellationToken cancellationToken) => Task.FromResult(this.runCommand(arguments));
    }
}
