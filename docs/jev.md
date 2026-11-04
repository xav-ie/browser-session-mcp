<img align="right" src="logo/jev-enabled.svg" width="140" alt="A holographic purple sticker reading jev inside, in the style of the old Intel inside stickers">

# Picking elements with Jev

_[← Back to README](../README.md) · [Docs index](README.md)_

[Jev](https://typesafe.ai) is TypeSafe's decision model. It can't plan or
write text, but it is fast and cheap at one thing: picking an option out of a
bounded set. Clicking through a page is mostly that, so browser-session-mcp
hands Jev the one decision an agent keeps paying a large model for: **which
element?**

- [Why use it](#why-use-it)
- [Setup](#setup)
- [Plain-language selectors](#plain-language-selectors)
- [act: a whole goal in one call](#act-a-whole-goal-in-one-call)
- [What Jev sees](#what-jev-sees)
- [How a pick works](#how-a-pick-works)
- [Limits and tips](#limits-and-tips)
- [Compared with other Jev browsers](#compared-with-other-jev-browsers)
- [Tuning](#tuning)

## Why use it

Without Jev, an agent acts on a page by reading a whole `take_snapshot`,
reasoning about it, and writing a CSS selector by hand. That is a large model
round trip per click, and the selector still breaks when the markup moves.

Jev replaces that with one small request:

| | a large model picks the element | Jev picks it |
|---|---|---|
| time per decision | seconds, and the snapshot grows the context every step | 70 to 500 ms end to end ([TypeSafe](https://typesafe.ai)) |
| input price | $0.20 to $10 per million tokens | $0.042 per million tokens, output free |
| cost of one pick here | the whole snapshot, every click | about $0.0007 (measured, [#34](https://github.com/xav-ie/browser-session-mcp/pull/34)) |
| survives markup changes | only if it rereads the page | yes, it picks from what is there now |

Published numbers from projects that put Jev in a browser loop:

- **[jev-ultrafast](https://github.com/browser-use/jev-ultrafast/blob/main/docs/performance.md)**
  (Browser Use): a Google Flights search, Zurich to London, in 7.07 s; 17 Jev
  calls at a median of 178 ms each.
- **[jev-browser (Ying-Kai-Liao)](https://github.com/Ying-Kai-Liao/jev-browser)**:
  40 of 42 live tasks right, with no false "done", at about 300 ms per call.
- **[TypeSafe's parallel questions cookbook](https://docs.typesafe.ai/cookbooks/parallel_questions.md)**:
  13 questions in one request cost 12.2 times less and ran 10 times faster
  than 13 separate requests. `act` relies on this: each step asks which
  element, whether the goal is met, whether it is stuck, whether a login or
  error page is in the way, and which value to type, all in one request.
- **[Jack & Jill case study](https://typesafe.ai/blog/jack-jill-jev-case-study)**:
  88% cheaper than Gemini, with one check going from about 3.6 s to about
  0.2 s.

TypeSafe's own headline is 193.6 times faster and 444.6 times cheaper than an
LLM; their blog notes the LLM side of that comparison is on the high end.

In this repo, live checks on Wikipedia picked "the link to Espresso" and "the
search box" at confidence 1.00, `act("open the Espresso article")` finished
in one step, and a nonsense goal was refused by the 0.85 fallback floor after
Jev scored a wrong element at 0.81.

The agent keeps the parts a large model is good at (planning, writing text,
checking the result) and stops spending tokens on "the login button is
`form > div:nth-child(3) > button`".

## Setup

Get a key at [console.typesafe.ai](https://console.typesafe.ai) and set it on
the server:

```sh
claude mcp add browser-session -e TYPESAFE_API_KEY=... -e BROWSER_URL=... -e STATE_DIR=... \
  -- nix run github:xav-ie/browser-session-mcp -- mcp
```

Without a key everything works as plain CSS, and `act` errors with a note
saying which variable to set.

## Plain-language selectors

`click`, `type` and `press_key` take a description wherever they take a CSS
selector:

```
click({ sessionId, selector: "the login button" })        ← tried as CSS first
click({ sessionId, selector: "jev: the login button" })   ← straight to Jev
type({ sessionId, selector: "the email field", text: "me@example.com" })
```

There is no syntax to learn. A selector is first used as CSS, and if it
matches nothing within `timeout`, it is retried as a plain-language goal.
That fallback also rescues a mistyped CSS selector when Jev can tell what it
meant. A `jev:` prefix skips the CSS step.

The tool's reply names what was picked, so the agent can check it:

```
Clicked button "Sign in" (jev: "the login button", confidence 0.97, 38 offered)
```

The two paths have different confidence floors. An explicit `jev:` goal asked
for a best guess, so it acts at 0.5 or above. The automatic fallback acts
only at 0.85 or above, because the caller asked for CSS and a rescue should
only fire when it is clearly right. (Jev will confidently pick something for
a goal that has no match on the page: 0.81 was measured on a nonsense target.)

`wait_for` takes CSS and text only. It polls; there is nothing to pick.

## act: a whole goal in one call

`act` runs the loop on the server. Each step is one Jev request that picks
the next element, or a scroll, wait or back, and in the same request judges
whether the goal is met, the agent is stuck, or a login or error page is in
the way:

```
act({
  sessionId,
  goal: "search for oolong tea and open the first result",
  values: { query: "oolong tea" },
  maxSteps: 8,
})
```

A step clicks a button or link, types into a field, picks a `<select>`
option, scrolls, waits or goes back. After each one the loop checks that
something changed (URL, title, visible text, open dialogs, checked and
expanded states, form values). If nothing did, it tries Jev's next best
choice, and after three dead actions in a row it stops.

Pass anything to type in `values`. Jev sees only the keys (`query`,
`password`) and says which field each one belongs in; the text goes straight
to the page over CDP, so secrets never reach a model.

It returns a trace of every step with its confidences, and a `status`:

| status | meaning | what to do |
|---|---|---|
| `completed` | Jev judged the goal met (0.85 or above) | check the page with `take_snapshot` or `take_screenshot`; Jev can call a goal met early |
| `stuck` | nothing on the page looks able to make progress (0.85 or above) | read the page and take over, or rephrase the goal |
| `needs_login` | a sign-in wall is in the way (0.5 or above) | `request_human_takeover` or `load_browser_state`, then call `act` again. Skipped when `values` were passed, since then the caller supplied what to type |
| `blocked` | the page shows an error or rejection (0.5 or above) | read the page |
| `no_progress` | three actions in a row changed nothing | read the page; something is probably covering it |
| `max_steps` | the budget ran out (`maxSteps`, default 8, at most 32) | call `act` again with the same goal, or raise `maxSteps` |

For one action, `click` with a plain-language selector is cheaper than `act`.

## What Jev sees

Each request carries:

- the goal, the page's URL and title, and its visible text (up to 1,500
  characters; an open dialog's text instead of the whole page's);
- one line per interactive element: tag, id, role, its label (aria label,
  placeholder, title, `<label>` or inner text), whether it is checked,
  expanded or selected, and a link's `href`;
- for `act`, the steps already taken and the keys of `values`.

What a person typed into a field is never sent. A text field or password box
is described by its label and `(filled)` or `(empty)` only. The page
fingerprint `act` uses to detect effects is hashed inside the page, so form
values never leave it either.

## How a pick works

<!-- diagram:jev-pick -->
<picture>
  <source media="(prefers-color-scheme: dark)" srcset="diagrams/jev-pick-dark.svg">
  <img src="diagrams/jev-pick.svg" alt="Sequence: the agent calls click with the plain-language selector the login button. The mcp server collects the page's interactive elements, up to 240 with the ones in the viewport first, and stamps each with a data-jev-id attribute. It sends Jev one Choice request with the goal, the page's text and the element list, and Jev answers e17 with confidence 0.97. Under the confidence floor, 0.5 or 0.85 on the fallback, it errors instead of guessing. The server clicks e17 with trusted CDP input and tells the agent it clicked the Sign in button">
</picture>
<!-- /diagram:jev-pick -->

1. Collect the page's interactive elements from the DOM (links, buttons,
   inputs, selects, editable regions and their ARIA roles), skipping hidden
   and disabled ones. Elements in the viewport go first, so a long page can't
   crowd them out; at most 240 are offered, under Jev's 255 option limit.
2. Stamp each one with a `data-jev-id` attribute carrying a random token for
   this pick.
3. Send one Choice request over those ids.
4. Act on `[data-jev-id="<token>-e17"]` through the same trusted CDP input as
   a CSS selector.

The random token matters when two callers share a tab: a newer pick re-stamps
the page, so an older answer matches nothing and fails, instead of landing on
whatever became `e17` since.

Jev only ever chooses among the ids it was just offered. Its answer never
becomes coordinates or JavaScript. Everything fails closed: no key, an empty
page, a confidence under the floor or a malformed answer is an error, never a
guess. On the automatic fallback, the error keeps the original CSS timeout so
the agent can see why Jev was tried.

## Limits and tips

- **Elements are read when the call runs.** On a page that is still loading,
  `wait_for` the content first.
- **Only interactive elements are offered.** "The third paragraph" has no
  match; describe something you can click or type into.
- **Past 240 elements**, the rest are left out and Jev is told how many.
  `act` can scroll to reach them; for a single click, `scroll` first.
- **Stealth trade off.** Collecting elements runs in an isolated world, but
  the `data-jev-id` stamps are DOM changes a page script could notice with a
  MutationObserver. Use CSS selectors on pages where that matters.
- **Verify the end state.** `completed` is Jev's opinion. Take a snapshot.

## Compared with other Jev browsers

Several projects drive a browser with Jev. They are mostly standalone
agents; browser-session-mcp adds Jev to a long-lived, logged-in session that
an agent already drives with ordinary tools, so the two mix in one flow.

| | browser-session-mcp | [jev-ultrafast](https://github.com/browser-use/jev-ultrafast) | [jev-browser (jkudish)](https://github.com/jkudish/jev-browser) | [jev-browser (Ying-Kai-Liao)](https://github.com/Ying-Kai-Liao/jev-browser) |
|---|---|---|---|---|
| form | MCP server, Rust, real Chrome over CDP | Python library | MCP, CLI and library on headless Playwright | MCP, CLI and library on Playwright |
| sessions and logins | persistent, isolated per agent, human takeover, saved cookies | per run | per run, password handoff | per run |
| single-element picks | `jev:` and automatic CSS fallback on click, type, press_key | no | no | `browser_choose` |
| text to type | caller's `values`, keys only to the model | a small LLM writes it | an LLM writes it | caller's values |
| stops on | goal met, stuck, login wall, error, no effect, budget | done, blocked | goal met, stuck | done, login, confirmation needed, ambiguous, error, blocked |
| page reads and checks | not yet | no | page as text, markdown or aria | `browser_check`, `browser_read` |

## Tuning

| var | default | notes |
|---|---|---|
| `TYPESAFE_API_KEY` | | turns Jev on |
| `JEV_MODEL` | `jev-1.14.0` | pinned on purpose: answers can shift between releases, so upgrade deliberately rather than riding `jev-latest` |
| `JEV_BASE_URL` | `https://api.typesafe.ai` | any `/v1/systemone` compatible carrier (Vercel AI Gateway, OpenRouter, Cloudflare) |
| `JEV_TIMEOUT_MS` | `10000` | per request |
| `JEV_MIN_CONFIDENCE` | `0.5` | floor for `jev:` goals and for each `act` step |
| `JEV_FALLBACK_MIN_CONFIDENCE` | `0.85` | floor for the automatic fallback |

The full list of server variables is in [Deployment](deployment.md#environment).
