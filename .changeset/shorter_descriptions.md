---
default: patch
---

# Shorter tool descriptions

Tools with an output schema no longer repeat their result shape in the description, so agents load fewer tokens per tool list. `start_profiling` and `stop_profiling` explain when to change `samplingIntervalUs`, `top` and `timeout`.
