# Local development workflow

- Keep the WSL checkout at `/home/jerome/dev/ai-agent-keeb` as the source of truth.
  `C:\dev\ai-agent-keeb` is an output folder, not another source checkout.
- Build the app with Windows Cargo through `build.ps1 -OutputRoot C:\dev\ai-agent-keeb`.
  From WSL, invoke that script with Windows PowerShell. Do not build the app
  with Linux Cargo or copy the source into the Windows output folder.
- The default mode tests, builds, installs, restarts, and verifies the copy in
  `%LOCALAPPDATA%\agent-frow`. Use that installed copy for testing.
  `-Mode Build` and `-Mode Test` are explicit alternatives; CI never installs.
- Versioned packages belong in `C:\dev\ai-agent-keeb\dist`. Use
  `dist.ps1 -OutputRoot C:\dev\ai-agent-keeb` to package a new release; preserve
  existing versioned archives. Do not create loose backup executables.
- The user handles Git pushes. Leave changes local unless explicitly told to push.

See `docs/how-it-works.md` for the layout, rollback behavior, and portable
commands. Other machines must supply their own separate Windows output root.
