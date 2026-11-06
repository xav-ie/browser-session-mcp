---
default: minor
---

# Plain-language picks reach every element, and start sooner

A pick used to see at most 240 elements, so a target further down a long
page could never be chosen. Jev now first sees what a person would (dialog
and viewport first) with the option to say the right element isn't there;
only then is the whole page offered, in even groups under its 255-option
limit. Jev can also answer "none of these", which is an error, not a guess.
Selectors that are plain words, like "the login button", go to Jev after one
CSS check instead of waiting out the whole `timeout`.
