#!/usr/bin/env sh
# Generates VS Code configuration: launch, tasks, settings and recommended extensions.
#
# .vscode/ is gitignored except extensions.json, so this script is the committed
# source of truth and the files it writes are local to each developer. Rerun it after
# pulling changes to it.
#
# Usage: sh scripts/development/setup-vscode.sh [--force]
#   --force   overwrite existing files; each is backed up to <name>.bak first
#
# Without --force, existing files are left untouched: people customise these.
set -eu

root=$(git rev-parse --show-toplevel)
dir="$root/.vscode"
force=0

for arg in "$@"; do
  case "$arg" in
    --force) force=1 ;;
    -h|--help) sed -n '2,12p' "$0" | sed 's/^# \{0,1\}//'; exit 0 ;;
    *) printf 'unknown argument: %s (try --help)\n' "$arg" >&2; exit 2 ;;
  esac
done

# NativeAOT cannot cross-compile, so the only RID this machine can publish is its own.
case "$(uname -s)" in
  Darwin) os=osx ;;
  MINGW*|MSYS*|CYGWIN*) os=win ;;
  *) os=linux ;;
esac
case "$(uname -m)" in
  arm64|aarch64) arch=arm64 ;;
  *) arch=x64 ;;
esac
rid="$os-$arch"

mkdir -p "$dir"
written=0
skipped=0

# write <name>: file content on stdin. @RID@ is substituted; ${...} is left for VS Code.
write() {
  target="$dir/$1"
  if [ -e "$target" ] && [ "$force" -ne 1 ]; then
    cat >/dev/null
    printf '  skip    .vscode/%s  (exists; --force to overwrite)\n' "$1"
    skipped=$((skipped + 1))
    return 0
  fi
  if [ -e "$target" ]; then
    cp "$target" "$target.bak"
    printf '  backup  .vscode/%s.bak\n' "$1"
  fi
  sed "s/@RID@/$rid/g" >"$target"
  printf '  wrote   .vscode/%s\n' "$1"
  written=$((written + 1))
}

printf 'Configuring VS Code in %s (host RID: %s)\n' "$dir" "$rid"

# ---------------------------------------------------------------------------------
write extensions.json <<'EOF'
{
  "recommendations": [
    "rust-lang.rust-analyzer",
    "vadimcn.vscode-lldb",
    "tamasfe.even-better-toml",
    "ms-dotnettools.csdevkit",
    "ms-dotnettools.csharp",
    "AvaloniaTeam.vscode-avalonia",
    "EditorConfig.EditorConfig",
    "tekumara.typos-vscode",
    "bierner.markdown-mermaid"
  ]
}
EOF

# ---------------------------------------------------------------------------------
write settings.json <<'EOF'
{
  // Build output is noise in search and a cost for the file watcher.
  "files.exclude": {
    "**/target": true,
    "**/bin": true,
    "**/obj": true
  },
  "files.watcherExclude": {
    "**/target/**": true,
    "**/bin/**": true,
    "**/obj/**": true
  },
  "search.exclude": {
    "**/target": true,
    "**/bin": true,
    "**/obj": true,
    "**/Cargo.lock": true,
    "**/packages.lock.json": true
  },
  "files.associations": {
    "*.axaml": "xml",
    "*.slnx": "xml",
    "*.props": "xml",
    "*.targets": "xml"
  },

  // .editorconfig is authoritative; these only keep the editor from fighting it.
  "files.insertFinalNewline": true,
  "files.trimTrailingWhitespace": true,
  "editor.formatOnSave": true,

  // Rust: show clippy (with the workspace's deny-level lints) as you type, so the
  // editor and CI disagree about nothing.
  "rust-analyzer.check.command": "clippy",
  "rust-analyzer.check.allTargets": true,
  "rust-analyzer.cargo.features": "all",
  "[rust]": {
    "editor.defaultFormatter": "rust-lang.rust-analyzer",
    "editor.rulers": [100]
  },

  // C#: analyse the whole solution, not just open files. The analyzers are
  // warnings-as-errors, so a violation in an unopened file still breaks the build.
  "dotnet.defaultSolution": "dotnet/MSBE.slnx",
  "dotnet.backgroundAnalysis.analyzerDiagnosticsScope": "fullSolution",
  "dotnet.backgroundAnalysis.compilerDiagnosticsScope": "fullSolution",
  "[csharp]": {
    "editor.defaultFormatter": "ms-dotnettools.csharp"
  },

  // Two trailing spaces are a hard line break in Markdown.
  "[markdown]": {
    "files.trimTrailingWhitespace": false
  }
}
EOF

# ---------------------------------------------------------------------------------
write tasks.json <<'EOF'
{
  "version": "2.0.0",
  "tasks": [
    {
      "label": "build: all",
      "dependsOn": ["cargo: build", "dotnet: build"],
      "dependsOrder": "parallel",
      "group": { "kind": "build", "isDefault": true },
      "problemMatcher": []
    },
    {
      "label": "test: all",
      "dependsOn": ["cargo: test", "dotnet: test"],
      "dependsOrder": "sequence",
      "group": { "kind": "test", "isDefault": true },
      "problemMatcher": []
    },

    {
      "label": "cargo: build",
      "type": "shell",
      "command": "cargo",
      "args": ["build", "--workspace"],
      "group": "build",
      "problemMatcher": ["$rustc"]
    },
    {
      "label": "cargo: clippy",
      "type": "shell",
      "command": "cargo",
      "args": ["clippy", "--workspace", "--all-targets", "--all-features", "--", "-D", "warnings"],
      "group": "build",
      "problemMatcher": ["$rustc"]
    },
    {
      "label": "cargo: test",
      "type": "shell",
      "command": "cargo",
      "args": ["test", "--workspace", "--all-features"],
      "group": "test",
      "problemMatcher": ["$rustc"]
    },
    {
      "label": "cargo: fmt",
      "type": "shell",
      "command": "cargo",
      "args": ["fmt", "--all"],
      "problemMatcher": []
    },

    {
      "label": "dotnet: build",
      "type": "process",
      "command": "dotnet",
      "args": [
        "build",
        "${workspaceFolder}/dotnet/MSBE.slnx",
        "/property:GenerateFullPaths=true",
        "/consoleloggerparameters:NoSummary"
      ],
      "group": "build",
      "problemMatcher": "$msCompile"
    },
    {
      "label": "dotnet: test",
      "type": "process",
      "command": "dotnet",
      "args": ["test", "${workspaceFolder}/dotnet/MSBE.slnx"],
      "group": "test",
      "problemMatcher": "$msCompile"
    },
    {
      "label": "dotnet: format",
      "type": "process",
      "command": "dotnet",
      "args": ["format", "whitespace", "${workspaceFolder}/dotnet/MSBE.slnx"],
      "problemMatcher": []
    },
    {
      // AOT breakage is invisible to `dotnet build`; this is how you see it locally.
      "label": "dotnet: publish NativeAOT (@RID@)",
      "type": "process",
      "command": "dotnet",
      "args": [
        "publish",
        "${workspaceFolder}/dotnet/src/MSBE.Desktop/MSBE.Desktop.csproj",
        "-c", "Release",
        "-r", "@RID@",
        "--self-contained"
      ],
      "problemMatcher": "$msCompile"
    },

    {
      "label": "check: xaml guards",
      "type": "shell",
      "command": "sh",
      "args": ["scripts/development/check-xaml.sh"],
      "problemMatcher": []
    },
    {
      "label": "check: everything (CI parity)",
      "type": "shell",
      "command": "sh",
      "args": ["scripts/development/check.sh"],
      "problemMatcher": ["$rustc", "$msCompile"]
    }
  ]
}
EOF

# ---------------------------------------------------------------------------------
write launch.json <<'EOF'
{
  "version": "0.2.0",
  "configurations": [
    {
      "name": "Rust: msbe (CLI)",
      "type": "lldb",
      "request": "launch",
      "cargo": {
        "args": ["build", "--package=msbe-cli", "--bin=msbe"],
        "filter": { "name": "msbe", "kind": "bin" }
      },
      "args": [],
      "cwd": "${workspaceFolder}"
    },
    {
      "name": "Rust: msbe-daemon",
      "type": "lldb",
      "request": "launch",
      "cargo": {
        "args": ["build", "--package=msbe-daemon", "--bin=msbe-daemon"],
        "filter": { "name": "msbe-daemon", "kind": "bin" }
      },
      "args": [],
      "cwd": "${workspaceFolder}"
    },
    {
      "name": "Rust: msbe-core unit tests",
      "type": "lldb",
      "request": "launch",
      "cargo": {
        "args": ["test", "--no-run", "--package=msbe-core", "--lib"],
        "filter": { "name": "msbe_core", "kind": "lib" }
      },
      "args": [],
      "cwd": "${workspaceFolder}"
    },
    {
      "name": ".NET: MSBE.Desktop",
      "type": "coreclr",
      "request": "launch",
      "preLaunchTask": "dotnet: build",
      "program": "${workspaceFolder}/dotnet/src/MSBE.Desktop/bin/Debug/net10.0/MSBE.Desktop.dll",
      "args": [],
      "cwd": "${workspaceFolder}/dotnet/src/MSBE.Desktop",
      "console": "internalConsole",
      "stopAtEntry": false
    }
  ],
  "compounds": [
    {
      // The desktop app is a client of the daemon; this is the everyday debug session.
      "name": "Daemon + Desktop",
      "configurations": ["Rust: msbe-daemon", ".NET: MSBE.Desktop"],
      "stopAll": true
    }
  ]
}
EOF

printf '\n%d written, %d skipped.\n' "$written" "$skipped"
if [ "$skipped" -gt 0 ]; then
  printf 'Rerun with --force to regenerate skipped files (originals are backed up to .bak).\n'
fi
printf 'Open the Extensions view and choose "Show Recommended Extensions" to install the toolchain.\n'
