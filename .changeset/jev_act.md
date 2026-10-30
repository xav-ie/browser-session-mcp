---
default: minor
---

# act: a Jev-driven goal loop

New `act({ sessionId, goal, values?, maxSteps? })` tool. The server extracts
the page's interactive elements, asks Jev for the next step (one fan-out
request also judging goal-met and stuck, gated at 0.85), executes it — click,
type, select, scroll or wait — and repeats until the goal reads achieved,
nothing can progress, or the step budget runs out. Values are typed via CDP;
only their keys reach the model.
