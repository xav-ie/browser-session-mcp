---
default: minor
---

# Throttle a session's CPU and network, and run init scripts

`set_emulation` slows a session's CPU (`cpuThrottlingRate`) and network (a
DevTools preset such as `slow4g`, or custom latency and bandwidth), and
`add_init_script` runs JavaScript before a page's own scripts on every load,
so agents can measure a page the way a slow phone sees it from the first
byte. Both apply to every tab of the session, including ones opened later,
and are in place when the tool returns; `get_emulation` and
`remove_init_script` read and undo them. They need the listener role.
