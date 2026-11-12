---
default: minor
---

# Watch the agent after a takeover, and take control again

- A takeover link now opens with the person in control, and every call on
  the session except reading logs fails with "a person has control" until
  they hand back, so nothing the agent does lands under them and nothing
  reads what they type.
- Done is now Hand back: the agent resumes and the link stays open in watch
  mode, a live view where clicks and keys are refused by the relay. Take
  control pauses the agent again. End closes the link, as does leaving the
  page closed for a minute.
- `await_human_takeover` returns on the handback with a new `watching` field,
  and fails on a token whose link ended and was already reported.
- While watching, a click, tap, key, scroll, touch drag or paste shows a
  hint to take control first.
- The takeover page reconnects by itself after a dropped connection, such as
  a phone pausing it.
