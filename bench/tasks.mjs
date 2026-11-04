// Benchmark tasks, from Ying-Kai Liao's jev-browser (MIT, Copyright (c) 2026
// Ying-Kai Liao): https://github.com/Ying-Kai-Liao/jev-browser/blob/main/bench/tasks.mjs
// Kept as close to the original as possible so results compare with theirs;
// the PEERS set adds single tasks other Jev browsers publish timings for.
// Each step's `assert` runs in the page afterwards and is the ground truth.
// `expect: "fail"` marks tasks that must not be reported done.
const TI = "https://the-internet.herokuapp.com";

export const TASKS = [
  // ---------------------------------------------------------------- forms & widgets
  { id: "ti-login", cat: "form", url: `${TI}/login`,
    steps: [{ goal: "Log in", values: { username: "tomsmith", password: "SuperSecretPassword!" }, assert: "() => location.pathname === '/secure'" }] },
  { id: "ti-dropdown", cat: "widget", url: `${TI}/dropdown`,
    steps: [{ goal: "Select Option 2 in the dropdown", assert: "() => document.querySelector('#dropdown').value === '2'" }] },
  { id: "ti-checkboxes", cat: "widget", url: `${TI}/checkboxes`,
    steps: [{ goal: "Make checkbox 1 checked and checkbox 2 unchecked", assert: "() => { const c = document.querySelectorAll('#checkboxes input'); return c[0].checked && !c[1].checked }" }] },
  { id: "ti-inputs", cat: "form", url: `${TI}/inputs`,
    steps: [{ goal: "Enter the number 42", values: { number: "42" }, assert: "() => document.querySelector('input[type=number]').value === '42'" }] },
  { id: "webform", cat: "form", url: "https://www.selenium.dev/selenium/web/web-form.html",
    steps: [{ goal: "Fill in the text input, password, textarea, dropdown and date fields with the given values, choose the second radio button, then submit the form",
      values: { text_input: "Jev", password: "pw123", textarea: "hello world", dropdown: "Two", date: "09/16/2026" }, maxActions: 14,
      assert: "() => { const q = new URLSearchParams(location.search); return q.get('my-text') === 'Jev' && q.get('my-password') === 'pw123' && q.get('my-textarea') === 'hello world' && q.get('my-select') === '2' && q.get('my-date') === '09/16/2026' && q.get('my-radio') === 'on' && location.pathname.includes('submitted') }" }] },
  { id: "webform-fine", cat: "form", url: "https://www.selenium.dev/selenium/web/web-form.html",
    steps: [
      { goal: "Type into the text input", values: { text: "Jev" } },
      { goal: "Type into the password field", values: { text: "pw123" } },
      { goal: "Type into the textarea", values: { text: "hello world" } },
      { goal: "Choose 'Two' in the dropdown select", values: { option: "Two" } },
      { goal: "Type the date into the date picker field", values: { text: "09/16/2026" } },
      { goal: "Select the second radio button" },
      { goal: "Submit the form",
        assert: "() => { const q = new URLSearchParams(location.search); return q.get('my-text') === 'Jev' && q.get('my-password') === 'pw123' && q.get('my-textarea') === 'hello world' && q.get('my-select') === '2' && q.get('my-date') === '09/16/2026' && q.get('my-radio') === 'on' }" },
    ] },

  // ---------------------------------------------------------------- dynamic pages
  { id: "ti-dynamic-loading", cat: "dynamic", url: `${TI}/dynamic_loading/2`,
    steps: [{ goal: "Start the loading and wait until the result text appears", assert: "() => document.querySelector('#finish')?.innerText.includes('Hello World')" }] },
  { id: "ti-dynamic-controls", cat: "dynamic", url: `${TI}/dynamic_controls`,
    steps: [{ goal: "Enable the text field, then type into it", values: { text: "hello" }, assert: "() => { const i = document.querySelector('#input-example input'); return !i.disabled && i.value === 'hello' }" }] },
  { id: "ti-add-remove", cat: "dynamic", url: `${TI}/add_remove_elements/`,
    steps: [{ goal: "Add elements until there are exactly 3 Delete buttons", assert: "() => document.querySelectorAll('.added-manually').length === 3" }] },
  { id: "todomvc-fine", cat: "spa", url: "https://demo.playwright.dev/todomvc/",
    steps: [
      { goal: "Add a new todo item that says 'buy milk'", values: { text: "buy milk" }, assert: "() => document.querySelectorAll('.todo-list li').length === 1" },
      { goal: "Add a new todo item that says 'walk the dog'", values: { text: "walk the dog" }, assert: "() => document.querySelectorAll('.todo-list li').length === 2" },
      { goal: "Mark the 'buy milk' todo as completed", assert: "() => [...document.querySelectorAll('.todo-list li.completed')].map(l => l.innerText.trim()).join('|') === 'buy milk'" },
      { goal: "Show only the completed todos", assert: "() => location.hash.includes('completed') && document.querySelectorAll('.todo-list li').length === 1" },
      { goal: "Clear all completed todos", assert: "() => document.querySelectorAll('.todo-list li').length === 0" },
    ] },
  { id: "todomvc-coarse", cat: "spa", url: "https://demo.playwright.dev/todomvc/",
    steps: [{ goal: "Add the two todos, mark 'buy milk' as completed, then clear completed todos", values: { first: "buy milk", second: "walk the dog" }, maxActions: 12,
      assert: "() => [...document.querySelectorAll('.todo-list li')].map(l => l.innerText.trim()).join('|') === 'walk the dog'" }] },

  // ---------------------------------------------------------------- interaction types
  { id: "ti-hover", cat: "interaction", url: `${TI}/hovers`,
    steps: [{ goal: "Reveal the name of the second user by hovering over their picture", assert: "() => getComputedStyle(document.querySelectorAll('.figcaption')[1]).display !== 'none'" }] },
  { id: "ti-js-confirm", cat: "interaction", url: `${TI}/javascript_alerts`,
    steps: [{ goal: "Trigger the JS Confirm dialog and accept it", assert: "() => document.querySelector('#result').innerText === 'You clicked: Ok'" }] },
  { id: "ti-entry-ad", cat: "interaction", url: `${TI}/entry_ad`,
    steps: [{ goal: "Close the advertisement modal", assert: "() => getComputedStyle(document.querySelector('#modal')).display === 'none'" }] },
  { id: "ti-sort-table", cat: "interaction", url: `${TI}/tables`,
    steps: [{ goal: "Sort the first table by Last Name", assert: "() => document.querySelector('#table1 tbody tr td').innerText === 'Bach'" }] },

  // ---------------------------------------------------------------- real sites / navigation
  { id: "hn-comments", cat: "navigation", url: "https://news.ycombinator.com/",
    steps: [{ goal: "Open the comments page of the top story", assert: "() => location.pathname === '/item'" }] },
  { id: "books-poetry", cat: "navigation", url: "https://books.toscrape.com/",
    steps: [
      { goal: "Open the Poetry category", assert: "() => location.pathname.includes('poetry')" },
      { goal: "Open the book \"Shakespeare's Sonnets\"", assert: "() => location.pathname.includes('shakespeares-sonnets')" },
    ] },
  { id: "wiki-search", cat: "large-page", url: "https://en.wikipedia.org/wiki/Main_Page",
    steps: [{ goal: "Search for the article and open it", values: { query: "Alan Turing" }, assert: "() => location.pathname === '/wiki/Alan_Turing'" }] },
  { id: "wiki-link", cat: "large-page", url: "https://en.wikipedia.org/wiki/Alan_Turing",
    steps: [{ goal: "Open the linked article about Bletchley Park", assert: "() => location.pathname === '/wiki/Bletchley_Park'" }] },
  { id: "github-pulls", cat: "large-page", url: "https://github.com/microsoft/playwright",
    steps: [{ goal: "Open the repository's Pull requests tab", assert: "() => /\\/pulls\\/?$/.test(location.pathname)" }] },
  { id: "saucedemo-fine", cat: "e2e", url: "https://www.saucedemo.com/",
    steps: [
      { goal: "Log in", values: { username: "standard_user", password: "secret_sauce" } },
      { goal: "Add the Sauce Labs Backpack to the cart" },
      { goal: "Open the cart and go to checkout" },
      { goal: "Fill in the checkout information and continue", values: { first_name: "Ada", last_name: "Lovelace", postal_code: "10001" } },
      { goal: "Finish the order", assert: "() => document.body.innerText.includes('Thank you for your order')" },
    ] },
  { id: "saucedemo-coarse", cat: "e2e", url: "https://www.saucedemo.com/",
    steps: [{ goal: "Log in, buy the Sauce Labs Backpack and complete the checkout", maxActions: 20,
      values: { username: "standard_user", password: "secret_sauce", first_name: "Ada", last_name: "Lovelace", postal_code: "10001" },
      assert: "() => document.body.innerText.includes('Thank you for your order')" }] },

  // ---------------------------------------------------------------- should NOT succeed
  { id: "neg-bad-password", cat: "negative", expect: "fail", url: `${TI}/login`,
    steps: [{ goal: "Log in", values: { username: "tomsmith", password: "wrong-password" }, assert: "() => location.pathname === '/secure'" }] },
  { id: "neg-missing-page", cat: "negative", expect: "fail", url: "https://books.toscrape.com/",
    steps: [{ goal: "Open the site's pricing page", assert: "() => location.pathname.includes('pricing')" }] },
  { id: "neg-disabled", cat: "negative", expect: "fail", url: "https://www.selenium.dev/selenium/web/web-form.html",
    steps: [{ goal: "Type into the disabled input field", values: { text: "x" }, assert: "() => document.querySelector('[name=my-disabled]').value === 'x'" }] },
];

// Harder tier: frames, shadow DOM, custom widgets, tabs, lazy content, and unsupported actions.
export const HARD = [
  { id: "iframe-form", cat: "iframe", url: "https://www.selenium.dev/selenium/web/iframes.html",
    steps: [{ goal: "Type the email address into the email field of the embedded form", values: { email: "jev@example.com" },
      assert: "() => document.querySelector('#iframe1').contentDocument.querySelector('#email').value === 'jev@example.com'" }] },
  { id: "iframe-datepicker", cat: "iframe", url: "https://jqueryui.com/datepicker/",
    steps: [{ goal: "Open the date picker and pick the 15th of the month it shows",
      assert: "() => /\\/15\\//.test(document.querySelector('iframe.demo-frame').contentDocument.querySelector('#datepicker').value)" }] },
  { id: "shadow-checkbox", cat: "shadow-dom", url: "https://www.selenium.dev/selenium/web/shadowRootPage.html",
    steps: [{ goal: "Check the checkbox", assert: "() => document.querySelector('custom-checkbox-element').shadowRoot.querySelector('input').checked" }] },
  { id: "react-select", cat: "custom-widget", url: "https://react-select.com/home",
    steps: [{ goal: "In the first dropdown on the page, choose the colour Purple",
      assert: "() => document.querySelector('[class*=singleValue]')?.innerText === 'Purple'" }] },
  { id: "new-window", cat: "tabs", url: "https://the-internet.herokuapp.com/windows",
    steps: [{ goal: "Open the new window", assert: "() => document.body.innerText.includes('New Window') && !location.pathname.endsWith('/windows')" }] },
  { id: "infinite-scroll", cat: "lazy", url: "https://the-internet.herokuapp.com/infinite_scroll",
    steps: [{ goal: "Scroll down to load more paragraphs", assert: "() => document.querySelectorAll('.jscroll-added').length >= 3" }] },
  { id: "github-issue-search", cat: "large-page", url: "https://github.com/microsoft/playwright/issues",
    steps: [{ goal: "Search this repository's issues for the given words", values: { query: "flaky screenshot" },
      assert: "() => decodeURIComponent(location.search).replace(/\\+/g, ' ').includes('flaky screenshot')" }] },
  // actions added after v3: drag, right-click, key press, upload
  { id: "drag", cat: "actions", url: "https://the-internet.herokuapp.com/drag_and_drop",
    steps: [{ goal: "Drag box A onto box B", assert: "() => document.querySelector('#column-a header').innerText === 'B'" }] },
  { id: "context-menu", cat: "actions", url: "https://the-internet.herokuapp.com/context_menu",
    steps: [{ goal: "Right-click inside the box to open the context menu", assertEvents: "context menu" }] },
  { id: "key-press", cat: "actions", url: "https://the-internet.herokuapp.com/key_presses",
    steps: [{ goal: "Press the Escape key", assert: "() => document.querySelector('#result').innerText === 'You entered: ESCAPE'" }] },
  // "upload" needs a file on the server's disk; act has no upload action.
  { id: "login-wall", cat: "negative", url: `${TI}/secure`,
    steps: [{ goal: "Open the secure area", expectStatus: "needs_login" }] },
  { id: "neg-missing-option", cat: "negative", expect: "fail", url: `${TI}/dropdown`,
    steps: [{ goal: "Select Option 7 in the dropdown", assert: "() => false" }] },
];

// Approval pause: with guard on, irreversible clicks must stop with needs_confirmation, and nothing else may.
export const GUARD = [
  { id: "guard-login", cat: "guard", guard: true, url: `${TI}/login`,
    steps: [{ goal: "Log in", values: { username: "tomsmith", password: "SuperSecretPassword!" }, assert: "() => location.pathname === '/secure'" }] },
  { id: "guard-webform", cat: "guard", guard: true, url: "https://www.selenium.dev/selenium/web/web-form.html",
    steps: [{ goal: "Type into the text input, then submit the form", values: { text: "Jev" }, assert: "() => location.pathname.includes('submitted')" }] },
  { id: "guard-checkout", cat: "guard", guard: true, url: "https://www.saucedemo.com/",
    steps: [
      { goal: "Log in", values: { username: "standard_user", password: "secret_sauce" } },
      { goal: "Add the Sauce Labs Backpack to the cart" },
      { goal: "Open the cart and go to checkout" },
      { goal: "Fill in the checkout information and continue", values: { first_name: "Ada", last_name: "Lovelace", postal_code: "10001" } },
      { goal: "Finish the order", expectStatus: "needs_confirmation", assert: "() => !document.body.innerText.includes('Thank you for your order')" },
    ] },
];

// Single tasks other Jev browsers publish numbers for.
export const PEERS = [
  // jkudish/jev-browser: about 4 s and 3.6 s.
  { id: "jk-coffee-espresso", cat: "peer", url: "https://en.wikipedia.org/wiki/Coffee",
    steps: [{ goal: "Open the article about Espresso", assert: "() => location.pathname === '/wiki/Espresso'" }] },
  { id: "jk-ristretto", cat: "peer", url: "https://en.wikipedia.org/wiki/Main_Page",
    steps: [{ goal: "Search Wikipedia for the espresso-based drink called Ristretto and stop when you are on that article", values: { query: "Ristretto" },
      assert: "() => location.pathname === '/wiki/Ristretto'" }] },
  // browser-use/jev-ultrafast: 2.798 s and 7.073 s. They generate the text
  // to type with an LLM; act takes it from `values`.
  { id: "ju-wiki-godel", cat: "peer", url: "https://en.wikipedia.org/wiki/Main_Page",
    steps: [{ goal: "Find and open the Wikipedia article about Gödel's incompleteness theorems.", values: { query: "Gödel's incompleteness theorems" },
      assert: "() => decodeURIComponent(location.pathname).startsWith('/wiki/Gödel') && location.pathname.includes('incompleteness')" }] },
  { id: "ju-flights", cat: "peer", url: "https://www.google.com/travel/flights?hl=en",
    steps: [{ goal: "Find one-way flights from Zurich to London on the given date and show the results.", values: { from: "Zurich", to: "London", date: "Dec 15" }, maxActions: 20,
      assert: "() => location.pathname.includes('/search') && /Zurich|ZRH/.test(document.body.innerText) && /London|LHR|LON/.test(document.body.innerText)" }] },
];
