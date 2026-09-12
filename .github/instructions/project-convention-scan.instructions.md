---
description: "Use before first edits to source code, tests, configuration, or documentation. Requires a quick local scan of project style and relevant documentation conventions."
name: "Pre-Edit Project Convention Scan"
---
# Pre-Edit Project Convention Scan

Before the first edit in a task, quickly identify the conventions that govern the target change.

- Read the target file and one nearby analogous implementation, test, or document section to establish local naming, formatting, error-handling, and documentation style.
- Check only directly applicable guidance: repository instruction files, the nearest relevant `README`, contributor guidance, and documentation adjacent to the feature. Do not perform a repository-wide documentation survey.
- State the local convention you will follow and use it in the first edit. When nearby evidence conflicts, prefer the closest ownership boundary and existing tests.
- If no clear convention is found after these targeted checks, preserve the surrounding file's style and make the smallest consistent change.
- Do this scan before editing, not as a substitute for focused validation after editing.

Example:

> Before editing `src/codec.rs`, inspect its module conventions, one neighboring codec test or implementation, and any provider API documentation that describes the wire format. Report only the rules that affect this change.
