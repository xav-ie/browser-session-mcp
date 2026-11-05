<img align="right" src="logo/jev-enabled.svg" width="140" alt="A holographic purple sticker reading jev inside, in the style of the old Intel inside stickers">

# Picking elements with Jev

_[← Back to README](../README.md) · [Docs index](README.md)_

[Jev](https://typesafe.ai) is TypeSafe's decision model. It can't plan or
write text, but it picks one option out of a list in a few hundred
milliseconds for a fraction of a cent. With a Jev key, browser-session-mcp
uses it so an agent can say what it wants instead of writing CSS selectors:

- **Plain-language selectors.** `click`, `type` and `press_key` accept a
  description like "the login button".
- **`act`.** Give it a goal and it works through the page on its own, one Jev
  decision per step.
- **`ask`.** A yes/no or multiple choice question about the page.
- **`read`.** Only the parts of a long page that answer a question.

The agent's own model still plans, writes text and checks results. Jev only
makes the many small "which one?" decisions, so those stop costing a large
model round trip each.

- [Setup](#setup)
- [Plain-language selectors](#plain-language-selectors)
- [act: a whole goal in one call](#act-a-whole-goal-in-one-call)
- [ask: questions about the page](#ask-questions-about-the-page)
- [read: only the relevant parts](#read-only-the-relevant-parts)
- [Approach](#approach)
- [What Jev sees](#what-jev-sees)
- [How a pick works](#how-a-pick-works)
- [Limits](#limits)
- [How it compares](#how-it-compares)
- [Tuning](#tuning)

## Setup

Get a key at [console.typesafe.ai](https://console.typesafe.ai) and set it on
the server:

```sh
claude mcp add browser-session -e TYPESAFE_API_KEY=... -e BROWSER_URL=... -e STATE_DIR=... \
  -- nix run github:xav-ie/browser-session-mcp -- mcp
```

Without a key, selectors are plain CSS and `act`, `ask` and `read` return an error
saying which variable to set. A pick costs about $0.0007.

## Plain-language selectors

`click`, `type` and `press_key` take a description wherever they take a CSS
selector:

```
click({ sessionId, selector: "the login button" })        ← tried as CSS first
click({ sessionId, selector: "jev: the login button" })   ← straight to Jev
type({ sessionId, selector: "the email field", text: "me@example.com" })
```

A selector is first tried as CSS. If it matches nothing, Jev picks the
element it describes, which also rescues a mistyped CSS selector. Real CSS
gets the full `timeout` to appear first; plain words like "the login button"
can never match as CSS, so they go to Jev right after one check. A `jev:`
prefix skips the CSS attempt. The reply names what was picked:

```
Clicked button "Sign in" (jev: "the login button", confidence 0.97, 38 offered)
```

When Jev isn't sure, nothing is clicked and the tool returns an error. A
`jev:` selector needs confidence 0.5. A CSS selector that fell back to Jev
needs 0.85, since the caller asked for CSS and Jev will confidently pick
something even for a target that isn't on the page (0.81 was measured on a
nonsense goal).

## act: a whole goal in one call

`act` runs the loop on the server: read the page, ask Jev for the next step,
do it, repeat.

<!-- diagram:act-loop -->
<picture>
  <source media="(prefers-color-scheme: dark)" srcset="diagrams/act-loop-dark.svg">
  <img src="diagrams/act-loop.svg" alt="Your agent calls act with a goal and values. act reads the page: its elements, visible text and the steps so far. It sends one Jev request that picks the next step and judges whether the goal is met, whether a wall is in the way and whether the click can't be undone. It does the step: click, type, hover, drag, select, scroll, or a key press. It checks the page changed, and if not tries Jev's next best pick. Then it loops back to read the page, until maxSteps or three actions in a row do nothing. When Jev's judgments say it's done, blocked or unsure, it stops with a status: completed or likely_completed; needs_login, needs_human or needs_confirmation; stuck, blocked or uncertain">
</picture>
<!-- /diagram:act-loop -->

```
act({
  sessionId,
  goal: "log in and open the billing page",
  values: { username: "ada", password: "..." },
})
```

Each step is one Jev request that picks what to do next and, in the same
request, judges whether the goal is met, whether a login, error or bot check
page is in the way, and whether the next click can't be undone. A step can
click, hover, right-click or drag an element, type into a field, pick a
`<select>` option, scroll, wait, go back, or press Enter or Escape.

- **Text to type goes in `values`.** Jev sees only the keys (`username`,
  `password`) and says which field each belongs in. The text goes straight to
  the page, so secrets never reach a model.
- **Every action is checked.** If the page didn't change, `act` tries Jev's
  next best choice, and stops after three dead actions in a row.
- **Dialogs and new tabs are handled.** JavaScript dialogs are accepted and
  listed in `dialogs`. A link that opens a new tab is followed there.
- **`confirmIrreversible: true`** stops before a click Jev judges can't be
  undone, such as placing an order or deleting something.
- **`maxSteps`** caps the run (default 8, at most 32).

It returns a trace of every step and a `status`:

| status | meaning | what to do |
|---|---|---|
| `completed` | Jev is confident the goal is met | check the page; Jev can be early |
| `likely_completed` | Jev thinks the goal is probably met | check the page before relying on it |
| `uncertain` | Jev wasn't sure of the next step | `candidates` lists what it was weighing; act on one yourself or rephrase the goal |
| `stuck` | nothing on the page looks able to help | read the page, or rephrase the goal |
| `needs_login` | a sign-in wall is in the way (ignored when `values` were passed) | `request_human_takeover` or `load_browser_state`, then call `act` again |
| `needs_human` | the page is a CAPTCHA or bot check | `request_human_takeover`, then call `act` again |
| `needs_confirmation` | with `confirmIrreversible`, the next click can't be undone | `pending` names it; click it yourself or call `act` again without the flag |
| `blocked` | the page shows an error | read the page |
| `no_progress` | three actions in a row changed nothing | read the page; something may be covering it |
| `max_steps` | the step budget ran out | call `act` again, or raise `maxSteps` |

For a single action, `click` with a plain-language selector is cheaper.

## ask: questions about the page

`ask` gets Jev's judgment of the page as it is now:

```
ask({ sessionId, question: "Is an error message showing?" })
  → { answer: "no", probability: 0.04 }

ask({ sessionId, question: "Which page is this?",
      options: ["product page", "cart", "checkout", "error page"] })
  → { answer: "cart", probability: 0.96, probabilities: [...] }
```

Without `options` the answer is yes or no, with the probability of yes. With
2 to 255 `options` it picks one, with each option's probability. Use it to
branch a script without a large model, or to check what `act` did. Jev is
weak at counting, arithmetic and dates, so read those with `evaluate` or
`take_snapshot` instead.

## read: only the relevant parts

`take_snapshot` returns the whole page. On a long article that is tens of
thousands of characters the agent has to read through. `read` returns only
what answers a question:

```
read({ sessionId, question: "When and why did Japan surrender?" })
```

The page's visible text is split into passages of about 600 characters, each
under its heading, leaving out navigation, sidebars, headers and footers.
Jev rates every passage's relevance at once, in parallel requests, and the
relevant ones come back in page order under `## heading` lines, up to
`maxChars` (12,000 by default; the most relevant first if they don't all
fit). Wikipedia's 118,000-character "Surrender of Japan" article comes back
as about 12,000 characters in under half a second. A page under 8,000
characters comes back whole, without asking Jev.

## Approach

Jev makes one kind of decision well: pick one option from a list, with a
calibrated confidence. Everything here is built around asking it that,
cheaply and often, and around what to do with an answer that isn't certain.

- **Show the page, not just the controls.** Every decision includes the
  page's visible text (an open dialog's first) next to the list of usable
  elements, so Jev can tell a result has loaded or a message appeared, not
  only what can be clicked.
- **Ask everything at once.** One `act` step is one request that asks which
  element, how to use it (click, hover, drag, right-click), which value to
  type into each field, whether the goal is met, whether a login, error or
  bot check page is in the way, and whether the click can't be undone. Jev
  answers extra questions in parallel, so they cost no waiting.
- **Stop when it's probably done.** Jev rarely rates a finished goal
  certain, and acting past a finished goal tends to undo it. So `act` stops
  once Jev rates the goal at even odds or better and reports
  `likely_completed`, except right after typing, when a search suggestion
  can look like the answer.
- **Never act on a guess.** Below its confidence floor, a selector errors
  and `act` stops with the candidates it was weighing. Jev may also answer
  "none of these", and the server only ever acts on an element it offered.
- **Check every action.** After each step `act` compares a fingerprint of
  the page. If nothing changed, it tries Jev's next best choice, and after
  three dead actions it stops instead of looping.
- **Type like a person.** Fields are cleared by selecting their text and
  typing over it, which works on React inputs; keys are pressed for real,
  and text that isn't on a US keyboard is inserted the way an input method
  does. The values themselves never reach Jev, only their names.
- **Offer everything, in the order a person sees it.** Elements in an open
  dialog come first, then the viewport, then the rest, including open shadow
  roots and same-origin iframes. A page with more than one question holds is
  split into even groups, so nothing is left out.
- **Leave no trace.** The elements offered are remembered in an isolated
  world the page's scripts can't see, so picking writes nothing to the page.

## What Jev sees

- The goal or question, the page's URL and title, and its visible text (an
  open dialog's text first). `act` and selectors send up to 1,500 characters
  of it, `ask` up to 20,000, and `read` all of it, a passage per question.
- For picks, one line per element: tag, id, role, its label, whether it is
  checked, expanded or selected, and a link's `href`.
- For `act`, the steps taken so far and the keys of `values`.

What a person typed into a field is never sent. A field is described by its
label and `(filled)` or `(empty)` only, and the page fingerprint `act` uses
to detect changes is hashed inside the page.

## How a pick works

<!-- diagram:jev-pick -->
<picture>
  <source media="(prefers-color-scheme: dark)" srcset="diagrams/jev-pick-dark.svg">
  <img src="diagrams/jev-pick.svg" alt="Sequence: the agent calls click with the plain-language selector the login button. The mcp server collects the page's interactive elements, those in a dialog and the viewport first, and keeps a reference to each where the page's scripts can't see it. It sends Jev one Choice request with the goal, the page's text and the element list, and Jev answers e17 with confidence 0.97. Under the confidence floor, 0.5 or 0.85 on the fallback, it errors instead of guessing. The server clicks e17 with trusted CDP input and tells the agent it clicked the Sign in button">
</picture>
<!-- /diagram:jev-pick -->

1. Collect the elements a person could use: links, buttons, form fields,
   ARIA widgets, draggable items, and things only a script makes clickable,
   like sortable table headers. Open shadow roots and same-origin iframes
   are included, and elements in an open dialog come first, then the rest
   of the viewport.
2. Keep a reference to each in an isolated world: a separate JavaScript
   space that shares the page's DOM but that the page's own scripts can't
   see. Nothing is written to the page, and an answer always lands on
   exactly the element it was offered, even if the page changed since.
3. Ask Jev to choose one id, or "none of these".
4. Act on it with the same real mouse and keyboard input as a CSS selector.

One Jev question holds at most 255 options. A page with more elements is
split into the fewest groups that fit, all the same size (1,800 elements
become 8 groups of 225), and one request asks every group for its best
match or none. Each group's top two go to the final pick, so nothing on the
page is left out. That costs one extra request, only on pages that big.

Jev only ever picks among the ids it was offered; its answer never becomes
coordinates or JavaScript.

## Limits

- **The page is read when the call runs.** On a page still loading,
  `wait_for` content first (`act` waits on its own).
- **Only usable elements are offered.** "The third paragraph" has no match.
- **Cross-origin iframes are out of reach**, such as a payment widget from
  another site.
- **Past 5,000 elements**, the rest are left out and Jev is told how many.
- **Verify the end state.** `completed` is Jev's opinion.

## How it compares

Other projects drive a browser with Jev too. They are standalone agents that
start a fresh browser per task. browser-session-mcp adds Jev to long-lived,
logged-in sessions the agent also drives with ordinary tools, with human
takeover for logins.

| | browser-session-mcp | [jev-browser](https://github.com/Ying-Kai-Liao/jev-browser) | [jev-browser (jkudish)](https://github.com/jkudish/jev-browser) | [jev-ultrafast](https://github.com/browser-use/jev-ultrafast) |
|---|---|---|---|---|
| runs as | MCP server on your Chrome | MCP server, CLI, library | MCP server, CLI, library | Python library |
| sessions | persistent, logged in, human takeover | one per task | one per task | one per task |
| picks one element | yes, on click, type and press_key | yes | no | no |
| text to type | your `values`, never shown to the model | your values | written by an LLM | written by an LLM |
| hover, drag, right-click | yes | yes | no | no |
| page questions | `ask` | yes | no | no |
| reads only the relevant part of a page | `read` | yes | no | no |

### Benchmark

jev-browser publishes a [suite of 40 tasks](https://github.com/Ying-Kai-Liao/jev-browser/blob/main/bench/tasks.mjs)
on live sites: logins, forms, widgets, loading pages, iframes, shadow DOM, and
tasks that should fail. Each task has a check that runs in the page. We ran
its own harness and ours (in [`bench/`](../bench)) on one machine, with the
same Jev model (`jev-1.14.0`), on 2026-11-04:

| | tasks done right | wrongly claimed done | typical time per task |
|---|---|---|---|
| browser-session-mcp | 87% (average of 3 runs) | 0 of 120 | 1.5 s |
| jev-browser | 81% (average of 2 runs) | 2 of 80 | 1.9 s |

Both miss tasks Jev can't judge, such as counting to exactly three or
reading a table's sort order. jev-browser does the work on a few tasks but
then reports itself stuck.

Other projects publish times for single tasks. Ours, on the same tasks:

| task | browser-session-mcp | published by |
|---|---|---|
| Wikipedia: from Coffee to the Espresso article | 1.3 s | about 4 s, jkudish/jev-browser |
| Wikipedia: search for Ristretto | 1.4 s | 3.6 s, jkudish/jev-browser |
| Wikipedia: search for Gödel's incompleteness theorems | 1.7 s | 2.8 s, jev-ultrafast |
| Google Flights: Zurich to London | about 6 s when it finishes, but the date calendar stops it most tries | 7.07 s, jev-ultrafast |

## Tuning

| var | default | notes |
|---|---|---|
| `TYPESAFE_API_KEY` | | turns Jev on |
| `JEV_MODEL` | `jev-1.14.0` | pinned on purpose: answers can shift between releases |
| `JEV_BASE_URL` | `https://api.typesafe.ai` | any `/v1/systemone` compatible provider (Vercel AI Gateway, OpenRouter, Cloudflare) |
| `JEV_TIMEOUT_MS` | `10000` | per request |
| `JEV_MIN_CONFIDENCE` | `0.5` | floor for `jev:` selectors |
| `JEV_FALLBACK_MIN_CONFIDENCE` | `0.85` | floor for CSS selectors that fell back to Jev |

`act` uses its own floor of 0.3, since it checks each action's effect and
retries with Jev's next best choice. The full list of server variables is in
[Deployment](deployment.md#environment).
