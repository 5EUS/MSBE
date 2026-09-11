#!/usr/bin/env sh
# Compile-time guards that MSBuild analyzers cannot express, because they live in
# XAML rather than C#. Run by the pre-commit hook and by CI.
set -eu

root=$(git rev-parse --show-toplevel)
status=0

# 1. ReflectionBinding breaks NativeAOT. BannedApiAnalyzers catches the C# form;
#    this catches the markup-extension form.
if hits=$(grep -rn "ReflectionBinding" "$root/dotnet" --include="*.axaml" 2>/dev/null); then
  printf 'error: ReflectionBinding is banned (breaks NativeAOT). Use compiled bindings with x:DataType.\n%s\n' "$hits" >&2
  status=1
fi

# 2. A view without x:DataType silently falls back to reflection bindings, which
#    is the exact failure mode that survives `dotnet build` and dies at publish.
for f in $(find "$root/dotnet" -name "*.axaml" 2>/dev/null); do
  case "$f" in
    */App.axaml|*/Styles/*|*/Themes/*) continue ;;
  esac
  if grep -q "{Binding" "$f" && ! grep -q "x:DataType" "$f"; then
    printf 'error: %s uses {Binding} without x:DataType; bindings will not compile.\n' "$f" >&2
    status=1
  fi
done

exit $status
