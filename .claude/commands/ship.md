---
description: Bump version, commit, tag, and push to trigger the GitHub Actions release build
---

Usage: `/ship v1.x.x`

Follow these steps in order:

1. Bump `version` in `Cargo.toml` to match the tag (e.g. `1.x.x`).

2. Stage all changes:
   ```powershell
   git add -A
   ```

3. Commit with the version as the message:
   ```powershell
   git commit -m "v1.x.x"
   ```

4. Create the tag:
   ```powershell
   git tag v1.x.x
   ```

5. Push the commit and the tag (ask the user for confirmation before running this step):
   ```powershell
   git push && git push origin v1.x.x
   ```
