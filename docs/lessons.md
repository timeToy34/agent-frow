# Things worth knowing before changing anything

The companion to [how-it-works.md](how-it-works.md): the constraints in this
codebase that were measured against real sessions rather than read from
documentation — in two cases the documentation is wrong — plus the history of
how each was learned. Read this before touching the state machine, focus, the
hook, or the lighting.

- **`PermissionRequest` and `Notification` carry no `tool_use_id`.** Only
  `session_id` and `prompt_id`, which are turn-level while a turn holds dozens
  of tool calls. Nothing can correlate a permission prompt to the tool call it
  belongs to. Do not build anything that needs to.
- **Claude prompts the user itself about six seconds after `PermissionRequest`.**
  Blocking that hook to collect an answer does not work; the tool proceeds
  regardless.
- **Every Codex hook blocks, and `async` is not usable.** Codex documents an
  `"async": true` flag and 0.148 accepts it, but 0.147 *skips any hook carrying
  it* — "async hooks are not supported yet" — so five of six events would
  silently never fire. We register plain synchronous hooks. That is safe: ours
  connects to loopback, writes a few hundred bytes and exits. What made the
  previous version unusable with Codex was the broker holding a hook open for
  ninety seconds, not the hook being synchronous.
- **Codex clamps `SessionEnd` to 3 seconds**, so we ask for exactly 3 rather
  than collecting a warning on its hooks screen.
- **Codex has no error and no interrupt event, and emits no `Notification` at
  all.** Its only "needs you" signal is `PermissionRequest`. Say so rather than
  guessing.
- **`PreToolUse` is 46% of all hook traffic and tells a lane nothing**, because
  agents auto-approve nearly every tool call. It is deliberately not registered.
- **Background subagents outlive the turn that spawned them.** `Stop` honestly
  means the main turn is done while subagents may still be at work, so the
  tracker keeps a per-session roster: `SubagentStart` enrols an `agent_id`,
  every event carrying it is its heartbeat, `SubagentStop` retires it, and 30
  minutes of silence retires one that died unannounced. A resting lane with a
  non-empty roster shows the Running pattern and "N subagents busy" — but
  Waiting, Error and Interrupted always win, because those need the user.
  Subagent events never change the lane's state itself: they carry the
  *parent's* `session_id`, and acting on them is how the old app cleared a
  Waiting nobody had answered.
- **Claude's `StopFailure` is how a lane can show an error.** An earlier version
  handled it but never registered it, so that state was unreachable.
- **Focus raises a window only after checking it is still what it was.**
  Matching any window an ancestor pid owned was subtly wrong: it could raise a
  transient Terminal helper (`PopupHost`), an unrelated app whose pid an
  ancestor's had been recycled into (a summon once raised iCUE), or — the tell
  — the F-row app's own window, which made the summon do nothing *only when
  the app was focused*, because the "already in front, leave it" shortcut then
  matched. The first guard was a terminal-class allowlist, which also ruled
  out agents living in a desktop app or an IDE. The guard now is identity: the
  hook records each ancestor's exe name at event time, and a window counts
  only while its pid still resolves to that name. When nothing qualifies it
  refuses and says so. Every summon raises the window; it skips only
  re-selecting an already-selected tab, which would move keyboard focus into
  the tab strip. A topmost/not-topmost Z-order move makes the
  terminal visibly frontmost without foreground permission, while an attached
  `SetForegroundWindow` still requests keyboard activation. Success is checked
  with the root window at the terminal's centre, not the foreground flag.
- **The hook must never print to stdout.** It is registered on `PermissionRequest`,
  which is a decision hook on both agents: anything it prints that parses could
  approve a real tool call. `hook/tests/silence.rs` asserts zero bytes.
- **The registered command string must never change.** Codex records trust
  against a hash of it. That is why it names `%LOCALAPPDATA%` rather than a build
  directory, and why the token lives in a file rather than in the command.

## Stories behind the rules

- **"Activity clears Waiting" is the whole design.** The lane that used to
  stick was caused by correlation: the previous application tried to match a
  permission prompt to the tool call it belonged to by `tool_use_id`, and the
  payloads carry no such id (first bullet above). Clearing Waiting on any
  activity means nothing has to be correlated to anything, so nothing can fail
  to correlate.
- **`PermissionDenied` exists in the registration because a rejected prompt's
  lane once sat on Waiting forever.** No hook fires at the moment of an
  interrupt, so a prompt dismissed by one left no evidence at all until the
  idle notification a minute later — which is deliberately ignored from
  Waiting, because an unanswered dialog is idle too.
- **Lighting sets only the twelve LEDs we own** because an earlier version
  returned an entry for every LED the device reported and then overwrote
  twelve of them, which is a way of spelling "black out the whole keyboard".
- **Focus was rebuilt three times, each time against a measured failure:**
  - *Foreground permission comes from the input, not force.* The keys were
    once swallowed in a `WH_KEYBOARD_LL` hook; a swallowed key reaches no
    window, so Windows granted no foreground permission, and summon failed
    precisely when the Agent F-Row window was focused — while a click on the
    Focus button, being real input to us, always worked. Registering F13–F24
    as global hotkeys makes the `WM_HOTKEY` input ours, and the raise is
    allowed. No synthetic input, no foreground-lock tricks.
  - *Activation and Z-order are separate.* `SetForegroundWindow` reported
    success while the window stayed drawn behind another; the summon claimed
    to work while nothing visibly moved. Hence the topmost/not-topmost
    `SetWindowPos` pair, and success judged by `WindowFromPoint` at the
    window's centre, never `GetForegroundWindow`.
  - *Focus runs off the window's thread* because UI Automation needs its own
    COM apartment — the windowing library has already put the UI thread into
    the other one, and UIA then reads no tabs at all — and because it can
    spend a quarter of a second waiting for the terminal to agree which tab
    is in front.
- **Two agents once came up reversed** — Agent A on B's lane and B on A's,
  each summon raising the other's window. Cause: lanes are claimed at first
  sight, hooks post concurrently, and a session adopted from a *subagent's*
  event deliberately carries no cwd (a subfolder must not become the lane's
  project) — so it could not be recognised as a saved agent and the fallback
  handed it the first free lane, somebody's preferred lane or not. Restarting
  the app made it likely: every live session re-adopts from whatever event
  lands first. Hence the one refinement in `claim()` (saved-agent preferences
  are otherwise plain first-come, by the owner's choice): a session whose cwd
  is still unknown takes a lane nobody prefers while there is one — and a
  laneless session re-enters assignment the moment its cwd arrives.
- **Never flip window styles behind winit's back.** Setting
  `WS_EX_TOOLWINDOW` directly to hide the taskbar button did not remove the
  button and left the restored window with a toolwindow caption — one lone
  close button — because winit caches window styles and reapplies its own
  idea of them. `ITaskbarList::DeleteTab/AddTab` removes the button without
  touching the frame.
- **The tray burned a full core doing nothing.** Hiding the window with
  `SW_HIDE` (`ViewportCommand::Visible(false)`) clears `WS_VISIBLE`, Windows
  then never delivers `WM_PAINT`, and eframe 0.33's scheduler — which dropped
  every due repaint into `ControlFlow::Poll` and relied on the resulting paint
  to re-arm `WaitUntil` — spun the event loop forever waiting for a paint that
  could not come (emilk/egui#7776). Measured: ~5% total CPU hidden, ~0.1%
  open, with the hidden process's working set trimmed to 5 MB so it looked
  "small but busy". The first fix was a detour: minimize instead of hide, pull
  the taskbar button with `ITaskbarList`, heal it in `update()` — a window
  that was still in Alt-Tab and still drawing twice a second. The real fix was
  upstream: eframe 0.34 never polls an invisible window (#7905) and runs no UI
  pass for one (`App::logic` / `App::ui`, #7950). So the tray is
  `Visible(false)` again and the 500 ms repaint lives only in `ui` — and is
  only re-armed while `IsWindowVisible` says so. That last part is ours to
  do: egui's idea of "visible" is "not minimized, not occluded", and Windows
  reports neither for `SW_HIDE`, so eframe kept running the full UI pass
  into the hidden window twice a second (UI thread: 185 ms per 30 s hidden
  against 108 ms per 15 s shown; with the gate, zero cycles in 30 s — the
  thread never runs). Two consequences: the window no longer sweeps while hidden
  — the lighting thread is the clock, as how-it-works.md says, and without the
  iCUE DLL stale sessions clear on the next reopen — and the title bar's
  minimize is just a minimize.
- **"Just unzip and run" silently did nothing** when any instance was already
  running: the app frees its console at the top of `run()`, and the port bind
  — which is also the single-instance lock — failed *after* that, printing
  "already in use" into a console that no longer existed. A double-click
  produced no window, no message, exit code 1. Hence the message box on a busy
  port, and the first-run bootstrap that made "unzip and run" true.
- **`install_binaries` compared paths textually.** The same file spelled two
  ways (case, an 8.3 short path) counted as different, so installing *from
  the installed copy* could rename the live exe aside and then fail to copy a
  source that no longer existed. Compare canonicalized paths.
- **A summon once raised iCUE.** A dead ancestor's pid had been recycled onto
  iCUE's process, and pid-only matching believed it. That is why every
  ancestor now carries its exe name from the hook's process snapshot, and a
  window only counts while its pid still resolves to that name.
- **A tab torn out of Windows Terminal was never found.** Terminal hosts every
  window in one process (that is what makes tear-out possible), so the torn-out
  window has the same pid as the one it left. Focus took the first terminal
  window of that pid in Z-order and asked UI Automation about that window
  only, so it raised the old window and reported the tab missing. Now every
  terminal window of the pid is kept, each one's tabs are read once, and the
  window showing — else holding — the tab is the one raised; nothing matching
  still raises the topmost, and the report lists every window's tabs.
  Mid-drag the tab is in no window's list, and the next press finds it.
- **The Keychron talks unasked.** A layer change arrives on the raw-HID
  interface as an `A3` report. An exchange that took "the next report" as its
  echo went one out of step for good — "the keyboard answered A3 to A8;
  reconnecting", forever, because every fresh handle met the next push. Read
  until the echo, skip the rest, drain before writing.
- **Quit skips Drop.** The tray's Quit is `process::exit`, so a surface that
  has to hand a keyboard back needs its own `restore_now()` on that path,
  exactly as the hotkeys needed `unhook_now()`.
- **Stock Keychron firmware ignores per-key brightness.** Only hue and
  saturation are per key; a black key is saturation 0 at the board's
  brightness — white. Found by reading the fork, confirmed on the board, fixed
  by building Keychron's fork with their open pull request and flashing it
  through the Launcher's hidden manual upload (the image is unsigned; the
  updater checks SHA-256 and the model string only).
- **The Keychron's Caps indicator is drawn, never cleared.** The firmware
  paints its lock indicator over the key while Caps is on and simply stops
  when it is off, trusting the key's effect to repaint it — so under an
  effect that never repaints the key, Caps stays lit forever. And "never
  repaints" is subtler than it sounds: a mixed-mode frame ends when *region
  0's* effect reports finished, and effect "none" is finished immediately,
  so with the user's lighting off only the board's first slice of 44 LEDs
  ever rendered. The first fix — move Caps Lock into the app's
  always-repainting region — failed on hardware exactly there: Caps Lock is
  LED 50, in the second slice, which never came; the F-row (LEDs 1–12) sits
  in the first, which is why it always worked and the starvation went
  unseen. What holds: with the lighting off, region 0 runs the per-key
  effect too, all its keys stored black — looks off, repaints everything,
  completes the frame. Not touched: the indicator-disable command, which
  would take the user's Caps light away.

## State of the work

- **M1 — tray app, agent detection, hook installation.** Done.
- **M2 — connect the hooks, state logic.** Done.
- **M3 — Corsair lighting**, plus its setup UI. Done.
- **M4 — click a lane to focus that agent's window and terminal tab.** Done.
- **M5 — Keychron Ultra lighting**, through the keyboard's own protocol. Done.

`app/src/surface/corsair/` and `app/src/focus/` are the only code carried over
from the previous version, because they were the only parts that demonstrably
worked. Both were rewritten around this application's own types on the way in.
