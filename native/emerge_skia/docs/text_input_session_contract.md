# Text Input Session Contract

`native/emerge_skia` now defines a small platform-neutral contract for host text input sessions in `src/events.rs`:

- `TextInputSessionAnchor`: the focused field rectangle in backend surface/render coordinates.
- `TextInputSession`: the host-visible session snapshot.
  - `content`
  - `cursor`
  - `selection_anchor`
  - `multiline`
  - `anchor`
- `TextInputSessionCommand`: backend commands for the host bridge.
  - `Show(session)`
  - `Hide`
  - `Update(session)`

This contract is intentionally narrower than `TextInputState`. It is only the backend-to-host bridge state needed to synchronize a focused text field with a platform IME, hidden text field, or equivalent native text system.

Editing semantics remain unchanged and continue to flow through the existing event-side types:

- `TextInputEditRequest`
- `TextInputCommandRequest`
- `TextInputPreeditRequest`
- `InputEvent`

## Compatibility Intent

Android now uses the shared session contract internally, but the existing JNI exports and behavior remain unchanged for Kotlin callers.

The current compatibility mapping is:

- `nativePollImeOp()` returns the legacy op codes.
  - `0` = none
  - `1` = show
  - `2` = hide
  - `3` = update
- `nativePollImeContent/Cursor/SelectionAnchor/Multiline/Anchor*()` continue to expose the latest session fields individually.
- `nativeOnTextCommit()` and `nativeOnKey()` remain unchanged.

This keeps the Android bridge stable while allowing other backends to consume the same named Rust session/command types without inheriting JNI-specific shapes.
